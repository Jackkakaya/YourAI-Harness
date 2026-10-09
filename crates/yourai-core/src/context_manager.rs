//! Context mutations are committed before the active in-memory view changes.
//!
//! 本模块同时持有 [`ContextManager`] 的公共压缩操作 [`Compactor::exec`]：
//! hook 生命周期、阻断、取消/提交竞态语义对全部实现固定，属于机制层。
use crate::prelude::{
    AbortReason, BaseInput, CommitStatus, ErrorKind, HookDispatchResult, HookEvent, HookInvocation,
    HookRuntime, ModelProvider, ProviderSnapshot, SessionContext, UsageTracker,
};
use crate::{chat::*, compaction::*, error::YourAiError, future::BoxFuture, session::*};
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio_util::sync::CancellationToken;

/// Dependencies selected once for this execution; context storage does not retain providers.
#[derive(Clone)]
pub struct Compactor {
    context: Arc<dyn ContextManager>,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
    usage: Option<Arc<dyn UsageTracker>>,
    hook_base: BaseInput,
    providers: Option<ProviderSnapshot>,
}
impl Compactor {
    pub fn new(
        context: Arc<dyn ContextManager>,
        model: Arc<dyn ModelProvider>,
        hooks: Option<Arc<dyn HookRuntime>>,
        usage: Option<Arc<dyn UsageTracker>>,
        hook_base: BaseInput,
    ) -> Self {
        Self {
            context,
            model,
            hooks,
            usage,
            hook_base,
            providers: None,
        }
    }
    pub fn from_snapshot(
        snapshot: &ProviderSnapshot,
        session: Option<&SessionContext>,
    ) -> Result<Self, YourAiError> {
        let context = snapshot
            .context_manager
            .clone()
            .ok_or_else(|| ErrorKind::Config("context not configured".into()))?;
        let mut hook_base = BaseInput::new(context.session_id().as_str(), "");
        if let Some(session) = session {
            hook_base.cwd = session.cwd.to_string_lossy().into_owned();
            hook_base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        Ok(Self {
            context,
            model: snapshot
                .model
                .clone()
                .ok_or_else(|| crate::ErrorKind::Config("model not configured".into()))?,
            hooks: snapshot.hooks.clone(),
            usage: snapshot.usage.clone(),
            hook_base,
            providers: Some(snapshot.clone()),
        })
    }
}

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub request: ChatRequest,
    pub estimated_tokens: u64,
    pub input_budget: Option<u64>,
    pub maintenance_needed: bool,
}
impl ContextRequest {
    pub fn fits(&self) -> bool {
        self.input_budget.is_none_or(|b| self.estimated_tokens <= b)
    }
}
pub trait ContextManager: Send + Sync {
    fn system_prompt(&self) -> String;
    fn session_id(&self) -> &SessionId;
    /// Synchronize previously started owned writes before publishing a readable view.
    /// Close uses this as the storage settlement barrier before releasing the session lease.
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>>;
    /// Appends stored messages atomically to the durable history.
    /// Retries reuse message identities; identical batches must not duplicate rows.
    /// Identity reuse with a different message or admission input must fail.
    ///
    /// Implementations must accept runtime-context rows (marker-prefixed
    /// notes such as `[PostCompact context]`): the public compact wrapper
    /// appends PostCompact additional contexts through this method, and a
    /// rejection would stop continuation after an already committed summary.
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        model: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError>;
    /// Implementation-side planning. Public context operations own the hook lifecycle.
    ///
    /// Hard invariants: `prepare_compaction` must not commit summary changes,
    /// and on the Summary path it must not mutate the active view — a
    /// PreCompact hook may still block afterwards. Summary commits happen
    /// only inside [`CompactionJob::run`]. Complete plans include only
    /// no-op/prune-only commits; Summary jobs hold any transaction/lock
    /// needed until their business commit finishes.
    fn prepare_compaction<'a>(
        &'a self,
        options: &'a CompactionRequest,
        model: &'a dyn ModelProvider,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>>;
    /// Read-only active view and archival identity checks used by recovery/deduplication.
    fn records(&self) -> Vec<StoredMessage>;
    /// Committed high-water mark, including messages removed from active context.
    fn last_sequence(&self) -> i64 {
        self.records().iter().map(|m| m.seq).max().unwrap_or(0)
    }
    fn messages(&self) -> Vec<ChatMessage> {
        self.records().into_iter().map(|r| r.message).collect()
    }
    fn contains_tool_call(&self, id: &str) -> bool {
        self.records().iter().any(|m| {
            m.message
                .content
                .tool_calls()
                .iter()
                .any(|c| c.call_id == id)
        })
    }
    fn contains_context_marker(&self, marker: &str) -> bool {
        self.records().iter().any(|m| {
            m.message
                .content
                .first_text()
                .is_some_and(|s| s.starts_with(marker))
        })
    }
    fn policy(&self) -> ContextPolicy {
        ContextPolicy::default()
    }
}

/// A prepared business operation; no hook protocol is required from implementations.
pub enum CompactionPlan<'a> {
    Complete(CompactionResult),
    Summary(Box<dyn CompactionJob + 'a>),
}

impl std::fmt::Debug for CompactionPlan<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Complete(result) => f.debug_tuple("Complete").field(result).finish(),
            Self::Summary(_) => f.debug_tuple("Summary").field(&"<compaction job>").finish(),
        }
    }
}

/// Implementation-side summary and durable commit.
pub trait CompactionJob: Send {
    /// Runs the summary and commits it durably.
    ///
    /// Set `committed` to CommitStatus::Started before an owned durable write,
    /// then CommitStatus::Committed immediately after the durable commit and
    /// before post-commit bookkeeping. A dropped waiter does not stop an owned
    /// storage worker. The wrapper reports an unknown commit while the write
    /// is still running and requires restore before continuation or retry.
    /// Implementations must recover their in-memory view after a dropped waiter.
    /// After a confirmed commit, bookkeeping failures return the committed tuple
    /// with a stop_reason, retaining the actual summary for PostCompact.
    /// Err still reports the commit stage, but cannot supply a PostCompact summary.
    fn run<'a>(
        self: Box<Self>,
        options: CompactionRequest,
        model: &'a dyn ModelProvider,
        usage: Option<&'a dyn UsageTracker>,
        cancel: &'a CancellationToken,
        committed: Arc<AtomicU8>,
    ) -> BoxFuture<'a, Result<(CompactionResult, String), YourAiError>>
    where
        Self: 'a;
}

// region:    --- 公共压缩操作（固定模板） ---

use crate::time::sleep_until;

fn op_error(message: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Provider {
        name: "compact",
        message: message.to_string(),
    }
    .into()
}

async fn dispatch_hook(
    execution: &Compactor,
    event: HookEvent,
    timeout: Option<Duration>,
) -> Result<HookDispatchResult, YourAiError> {
    let mut invocation = HookInvocation::new(execution.hook_base.clone(), event);
    if let Some(providers) = &execution.providers {
        invocation = invocation.with_providers(providers);
    }
    invocation.model = Some(execution.model.clone());
    invocation.usage = execution.usage.clone();
    let kind = invocation.event.kind();
    let result = match &execution.hooks {
        Some(hooks) => crate::time::timeout(timeout, hooks.dispatch(&invocation))
            .await
            .map_err(|_| op_error("hook deadline exceeded"))??,
        None => HookDispatchResult::empty(kind),
    };
    result.validate_for(kind)?;
    Ok(result)
}

/// Fixed public compaction operation shared by every [`ContextManager`] implementation.
///
/// 编排顺序：业务准备 → PreCompact（仅摘要路径，可阻断）→ 摘要提交 → PostCompact。
///
/// - 未变更与只剪枝的操作不触发摘要 hook（`CompactionPlan::Complete` 快路径）。
/// - PreCompact 阻断发生在任何 active view 变更之前（prepare 的硬性不变量）。
/// - PostCompact 阻断或失败只设置 `stop_reason`，不撤销已提交摘要。
/// - 取消/超时与业务提交竞态时，通过提交进度区分取消、提交状态未知和已提交后中断。
///
/// 直接调用 `prepare_compaction` / [`CompactionJob::run`] 属于实现协议，
/// 不会获得本入口的 hook 与竞态契约。
impl Compactor {
    async fn run<'a>(
        &'a self,
        job: Box<dyn CompactionJob + 'a>,
        options: CompactionRequest,
        cancel: &'a CancellationToken,
        committed: Arc<AtomicU8>,
    ) -> Result<(CompactionResult, String), YourAiError> {
        job.run(
            options,
            self.model.as_ref(),
            self.usage.as_deref(),
            cancel,
            committed,
        )
        .await
    }
    /// Public execution owns PreCompact, business run, and PostCompact.
    pub async fn exec(
        &self,
        options: CompactionRequest,
        cancel: &CancellationToken,
        hook_timeout: Option<Duration>,
    ) -> Result<CompactionResult, YourAiError> {
        let execution = self;
        let context = self.context.as_ref();
        let committed = Arc::new(AtomicU8::new(CommitStatus::Pending as u8));
        let deadline = options.deadline;
        let operation = async {
            let plan = context
                .prepare_compaction(&options, self.model.as_ref(), cancel)
                .await?;
            let job = match plan {
                CompactionPlan::Complete(result) => {
                    if result.action == CompactAction::Summarized {
                        return Err(ErrorKind::Config(
                            "summary commits must use a CompactionJob".into(),
                        )
                        .into());
                    }
                    return Ok(result);
                }
                CompactionPlan::Summary(job) => job,
            };
            let trigger = match options.trigger {
                CompactionTrigger::Manual => "manual",
                CompactionTrigger::Threshold => "auto",
                CompactionTrigger::Overflow => "overflow",
            };
            let pre = dispatch_hook(
                execution,
                HookEvent::PreCompact {
                    trigger: trigger.into(),
                    custom_instructions: options.custom_instructions.clone(),
                },
                hook_timeout,
            )
            .await?;
            if pre.common.prevent_continuation {
                return Err(AbortReason::HookStopped(
                    pre.common
                        .stop_reason
                        .clone()
                        .unwrap_or_else(|| "PreCompact stopped".into()),
                )
                .into());
            }
            if !pre.common.blocking_errors.is_empty() {
                return Err(op_error("PreCompact blocked summary"));
            }
            let mut run_options = options.clone();
            let extra = pre.additional_contexts().to_vec();
            if !extra.is_empty() {
                run_options.custom_instructions = Some(
                    [
                        run_options.custom_instructions.unwrap_or_default(),
                        extra.join("\n"),
                    ]
                    .join("\n"),
                );
            }
            let (mut result, summary) = self
                .run(job, run_options, cancel, committed.clone())
                .await?;
            committed.store(CommitStatus::Committed as u8, Ordering::Release);
            if result.action != CompactAction::Summarized {
                return Err(ErrorKind::Config(
                    "CompactionJob must report a Summarized commit; the summary is durable, \
                 but its reported action breaks overflow accounting"
                        .into(),
                )
                .into());
            }
            result.notices.extend(pre.notices().map(str::to_owned));
            match dispatch_hook(
                execution,
                HookEvent::PostCompact {
                    trigger: trigger.into(),
                    compact_summary: summary,
                },
                hook_timeout,
            )
            .await
            {
                Ok(post) => {
                    result.notices.extend(post.notices().map(str::to_owned));
                    let additional = post.additional_contexts().to_vec();
                    if !additional.is_empty() {
                        if let Err(e) = context
                            .append(vec![StoredMessage::runtime_context(format!(
                                "[PostCompact context]\n{}",
                                additional.join("\n")
                            ))])
                            .await
                        {
                            result.stop_reason.get_or_insert_with(|| {
                                format!("summary committed; PostCompact context save failed: {e}")
                            });
                        }
                    }
                    if post.common.prevent_continuation || !post.common.blocking_errors.is_empty() {
                        result.stop_reason.get_or_insert_with(|| {
                            post.common.stop_reason.unwrap_or_else(|| {
                                "summary committed; PostCompact stopped continuation".into()
                            })
                        });
                    }
                }
                Err(e) => {
                    result.stop_reason.get_or_insert_with(|| {
                        format!("summary committed; PostCompact failed: {e}")
                    });
                }
            }
            Ok(result)
        };
        tokio::select! {
            biased;
            _ = cancel.cancelled() => Err(commit_failure(&committed, AbortReason::Cancelled.into())),
            _ = sleep_until(deadline) => Err(commit_failure(&committed, AbortReason::DeadlineExceeded.into())),
            result = operation => result.map_err(|cause| commit_failure(&committed, cause)),
        }
    }
}
// endregion: --- 公共压缩操作（固定模板） ---

fn commit_failure(progress: &AtomicU8, cause: YourAiError) -> YourAiError {
    match progress.load(Ordering::Acquire) {
        value if value == CommitStatus::Committed as u8 => op_error(format!(
            "summary committed; post-commit operation failed: {cause}"
        )),
        value if value == CommitStatus::Started as u8 => op_error(format!(
            "summary commit state unknown; restore before continuing; do not replay: {cause}"
        )),
        _ => cause,
    }
}
