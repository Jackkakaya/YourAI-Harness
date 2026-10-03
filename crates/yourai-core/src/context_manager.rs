//! Context mutations are committed before the active in-memory view changes.
//!
//! 本模块同时持有 [`ContextManager`] 的公共压缩操作 [`compact`]：
//! hook 生命周期、阻断、取消/提交竞态语义对全部实现固定，属于机制层。
use crate::prelude::{
    AbortReason, BaseInput, ErrorKind, HookDispatchResult, HookEvent, HookInvocation,
    HookPointOutcome, HookRuntime, ModelProvider, ProviderSnapshot, SessionContext, UsageTracker,
};
use crate::{chat::*, compaction::*, error::YourAiError, future::BoxFuture, session::*};
use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio_util::sync::CancellationToken;

/// Dependencies selected once for this execution; context storage does not retain providers.
#[derive(Clone)]
pub struct ContextExecution {
    pub model: Arc<dyn ModelProvider>,
    pub hooks: Option<Arc<dyn HookRuntime>>,
    pub usage: Option<Arc<dyn UsageTracker>>,
    pub hook_base: BaseInput,
}
impl ContextExecution {
    pub fn from_snapshot(
        snapshot: &ProviderSnapshot,
        id: &SessionId,
        session: Option<&SessionContext>,
    ) -> Result<Self, YourAiError> {
        let mut hook_base = BaseInput::new(id.as_str(), "");
        if let Some(session) = session {
            hook_base.cwd = session.cwd.to_string_lossy().into_owned();
            hook_base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        Ok(Self {
            model: snapshot
                .model
                .clone()
                .ok_or_else(|| crate::ErrorKind::Config("model not configured".into()))?,
            hooks: snapshot.hooks.clone(),
            usage: snapshot.usage.clone(),
            hook_base,
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
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>>;
    /// Appends stored messages to the durable history.
    ///
    /// Implementations must accept runtime-context rows (marker-prefixed
    /// notes such as `[PostCompact context]`): the public compact wrapper
    /// appends PostCompact additional contexts through this method, and a
    /// rejection would stop continuation after an already committed summary.
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn build_request(
        &self,
        tools: &[Tool],
        execution: &ContextExecution,
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
        execution: &'a ContextExecution,
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
    fn default_options(&self) -> ChatOptions {
        ChatOptions::default()
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

#[derive(Debug)]
pub struct CompactionCommit {
    pub result: CompactionResult,
    pub summary: String,
}

/// Implementation-side summary and durable commit.
pub trait CompactionJob: Send {
    /// Runs the summary and commits it durably.
    ///
    /// `committed` must be set to `true` as soon as the business commit is
    /// durable — before post-commit bookkeeping or returning (see
    /// `MemoryContext::run_summary`). The wrapper reads it only after a
    /// cancellation/deadline drop: a set flag means the summary landed even
    /// though this future was abandoned, so the caller reports the commit
    /// instead of a plain cancellation. Implementations must tolerate the
    /// future being dropped between the durable write and resolution; the
    /// `MemoryContext` recovery protocol (dirty-flag reload) is one way to
    /// make a misreported cancellation recoverable.
    fn run<'a>(
        self: Box<Self>,
        options: CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
        committed: &'a std::sync::atomic::AtomicBool,
    ) -> BoxFuture<'a, Result<CompactionCommit, YourAiError>>
    where
        Self: 'a;
}

// region:    --- 公共压缩操作（固定模板） ---

/// Optional policy bounds: absence of a timer never disables caller cancellation.
async fn bounded_timeout<T>(
    duration: Option<Duration>,
    future: impl Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    match duration {
        Some(duration) => tokio::time::timeout(duration, future).await,
        None => Ok(future.await),
    }
}

async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

fn op_error(message: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Provider {
        name: "compact",
        message: message.to_string(),
    }
    .into()
}

async fn dispatch_hook(
    execution: &ContextExecution,
    event: HookEvent,
    timeout: Option<Duration>,
) -> Result<HookDispatchResult, YourAiError> {
    let kind = event.kind();
    let result = match &execution.hooks {
        Some(hooks) => bounded_timeout(
            timeout,
            hooks.dispatch(&HookInvocation::new(execution.hook_base.clone(), event)),
        )
        .await
        .map_err(|_| op_error("hook deadline exceeded"))??,
        None => HookDispatchResult::empty(kind),
    };
    result.validate_for(kind)?;
    Ok(result)
}

fn additional_contexts(r: &HookDispatchResult) -> Vec<String> {
    match &r.outcome {
        HookPointOutcome::Generic(o) => o.additional_contexts.clone(),
        _ => vec![],
    }
}

/// Fixed public compaction operation shared by every [`ContextManager`] implementation.
///
/// 编排顺序：业务准备 → PreCompact（仅摘要路径，可阻断）→ 摘要提交 → PostCompact。
///
/// - 未变更与只剪枝的操作不触发摘要 hook（`CompactionPlan::Complete` 快路径）。
/// - PreCompact 阻断发生在任何 active view 变更之前（prepare 的硬性不变量）。
/// - PostCompact 阻断或失败只设置 `stop_reason`，不撤销已提交摘要。
/// - 取消/超时与业务提交竞态时，通过 `committed` 标志区分"已提交但中断"与纯取消。
///
/// 直接调用 `prepare_compaction` / [`CompactionJob::run`] 属于实现协议，
/// 不会获得本入口的 hook 与竞态契约。
pub async fn compact(
    context: &dyn ContextManager,
    options: CompactionRequest,
    execution: &ContextExecution,
    cancel: &CancellationToken,
    hook_timeout: Option<Duration>,
) -> Result<CompactionResult, YourAiError> {
    let committed = AtomicBool::new(false);
    let deadline = options.deadline;
    let operation = async {
        let plan = context
            .prepare_compaction(&options, execution, cancel)
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
        let extra = additional_contexts(&pre);
        if !extra.is_empty() {
            run_options.custom_instructions = Some(
                [
                    run_options.custom_instructions.unwrap_or_default(),
                    extra.join("\n"),
                ]
                .join("\n"),
            );
        }
        let CompactionCommit {
            mut result,
            summary,
        } = job.run(run_options, execution, cancel, &committed).await?;
        committed.store(true, Ordering::Release);
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
                let additional = additional_contexts(&post);
                if !additional.is_empty() {
                    if let Err(e) = context
                        .append(vec![StoredMessage::runtime_context(format!(
                            "[PostCompact context]\n{}",
                            additional.join("\n")
                        ))])
                        .await
                    {
                        result.stop_reason = Some(format!(
                            "summary committed; PostCompact context save failed: {e}"
                        ));
                    }
                }
                if post.common.prevent_continuation || !post.common.blocking_errors.is_empty() {
                    result.stop_reason = Some(post.common.stop_reason.unwrap_or_else(|| {
                        "summary committed; PostCompact stopped continuation".into()
                    }));
                }
            }
            Err(e) => {
                result.stop_reason = Some(format!("summary committed; PostCompact failed: {e}"))
            }
        }
        Ok(result)
    };
    tokio::select! {
        biased;
        _ = cancel.cancelled() => if committed.load(Ordering::Acquire) { Err(op_error("summary committed; cancelled during PostCompact")) } else { Err(AbortReason::Cancelled.into()) },
        _ = sleep_until(deadline) => if committed.load(Ordering::Acquire) { Err(op_error("summary committed; deadline exceeded during PostCompact")) } else { Err(AbortReason::DeadlineExceeded.into()) },
        result = operation => result,
    }
}
// endregion: --- 公共压缩操作（固定模板） ---
