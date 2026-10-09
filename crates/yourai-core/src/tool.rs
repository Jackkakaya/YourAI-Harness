//! 工具：定义侧（给模型的 schema）与执行侧（ToolContext 赋能）分离。
//!
//! 执行签名带 [`ToolContext`]——工具有自己的身份（call_id）、能发进度、
//! 能被取消、能拿到两层审批能力（决策 5.5 + 收敛修订）。
//!
//! **两层审批**（架构收敛结论）：
//! - 第一层（公共执行包装）：`check_tool_call`——执行前统一问
//! - 第二层（具体工具）：`check_command` / `check_file_access` 与
//!   `sandbox.apply(&mut Command)`——只有工具自己知道 Command/路径，
//!   由工具经 [`ToolContext`] 注入的能力自行调用
//!
//! 工具仍然不知道 UI 存在——只对 outbox 讲协议（Ask/Reply 经执行层中介）。

use crate::chat::{ToolDefinition, ToolResponse};
use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::interaction::Bridge;
use crate::interaction::{InteractionKind, InteractionRequest, ToolInteraction};
use crate::prelude::*;
use crate::sandbox::SandboxProvider;
use crate::security::{SecurityContext, SecurityProvider};
use crate::ui::OutSink;
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

/// 工具执行期的能力注入（由公共执行包装装配）。
pub struct ToolContext<'a> {
    /// Providers frozen for this turn, also used when assembling inherited children.
    pub providers: Option<&'a ProviderSnapshot>,
    /// 本次工具调用的身份（= 模型 ToolCall 的 call_id）；
    /// 工具进度事件用此 id；Ask 使用独立 request_id 并关联此调用
    pub call_id: String,
    /// Working directory frozen from the current turn; None uses provider defaults.
    pub cwd: Option<&'a std::path::Path>,
    /// 发 Out（约定：工具只发 `Out::ToolProgress`；subagent 转发子事件也走这里）
    pub emit: &'a dyn OutSink,
    /// ESC 打断长工具
    pub cancel: &'a CancellationToken,
    /// 第二层审批：命令/文件级检查（快照，可能为 None = 不拦截）
    pub security: Option<Arc<dyn SecurityProvider>>,
    /// 沙箱：工具对自建的 `tokio::process::Command` 调 `apply`（快照）
    pub sandbox: Option<Arc<dyn SandboxProvider>>,
    /// 工具执行期间请求用户交互；执行包装同时服务其内部请求通道。
    /// None 表示不支持交互，ask() 立即报 Config，而不是永久等待。
    pub interaction: Option<&'a dyn ToolInteraction>,
}

impl ToolContext<'_> {
    /// 发一条进度事件（id 自动取 call_id）；返回 false = 消费端已关闭
    pub fn emit_progress(&self, payload: Value) -> bool {
        self.emit.send(crate::protocol::Out::ToolProgress {
            id: self.call_id.clone(),
            payload,
        })
    }

    /// 向执行层提问，自动生成独立请求 id 并绑定本次工具调用。
    pub fn ask<'a>(
        &'a self,
        kind: InteractionKind,
        payload: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            if self.cancel.is_cancelled() {
                return Err(crate::AbortReason::Cancelled.into());
            }
            let interaction = self.interaction.ok_or_else(|| {
                crate::ErrorKind::Config("tool interaction is not configured".into())
            })?;
            let request = InteractionRequest::new(self.call_id.clone(), kind, payload);
            tokio::select! {
                biased;
                _ = self.cancel.cancelled() => Err(crate::AbortReason::Cancelled.into()),
                result = interaction.request(request, self.cancel) => result,
            }
        })
    }
}

/// Implementation-side backend. Scheduling code uses Tool::exec.
pub trait ToolProvider: Send + Sync {
    /// 给模型看的 schema（genai::chat::Tool）
    fn definition(&self) -> ToolDefinition;

    /// 为第一层审批描述这次调用。
    ///
    /// 执行包装在执行前调用本方法，再把结果交给
    /// `SecurityProvider::check_tool_call`。工具必须显式标注破坏性和网络属性，
    /// 避免执行包装根据名字猜测安全语义。
    fn security_context(&self, input: &Value, cwd: Option<&std::path::Path>) -> SecurityContext;

    /// 普通错误由执行包装转成工具失败结果；Turn 取消/总超时/额度耗尽向上收尾，
    /// 不能把所有 Aborted 都转换成普通工具错误继续模型循环。
    /// future 必须 cancellation-safe：取消/超时时执行包装会取消子 token，宽限后丢弃 future。
    /// 实现须通过 RAII 或有界清理回收自身进程/任务，不能脱离 Turn 留下执行。
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

/// Registered execution handle. The implementation backend is held privately.
#[derive(Clone)]
pub struct Tool {
    backend: Arc<dyn ToolProvider>,
    definition: Arc<ToolDefinition>,
}
impl Tool {
    pub fn new(backend: Arc<dyn ToolProvider>) -> Self {
        Self {
            definition: Arc::new(backend.definition()),
            backend,
        }
    }
    pub fn name(&self) -> &str {
        self.definition.name.as_str()
    }
    pub fn definition(&self) -> ToolDefinition {
        (*self.definition).clone()
    }
    pub fn security_context(
        &self,
        input: &Value,
        cwd: Option<&std::path::Path>,
    ) -> SecurityContext {
        self.backend.security_context(input, cwd)
    }
    pub async fn exec(
        &self,
        turn: &mut crate::execution::Turn<'_>,
        call_id: &str,
    ) -> Result<ExecutedTool, YourAiError> {
        use crate::execution::CallState;
        turn.ensure_active()?;
        let record = turn
            .calls
            .iter_mut()
            .find(|record| record.call.call_id == call_id)
            .ok_or_else(|| ErrorKind::Tool {
                name: call_id.into(),
                message: "tool call is not pending".into(),
            })?;
        let bound = record.tool.as_ref().ok_or_else(|| ErrorKind::Tool {
            name: record.call.fn_name.clone(),
            message: "tool is not bound".into(),
        })?;
        if !Arc::ptr_eq(&self.definition, &bound.definition)
            || !matches!(record.state, CallState::Pending)
        {
            return Err(ErrorKind::Tool {
                name: record.call.fn_name.clone(),
                message: "tool binding differs or call has already started; do not replay".into(),
            }
            .into());
        }
        record.state = CallState::Running;
        let mut call = record.call.clone();
        let mut cleanup_deadline = None;
        turn.send(Out::ToolStarted {
            id: call.call_id.clone(),
            name: call.fn_name.clone(),
            input: call.fn_arguments.clone(),
        })?;
        let result = async {
            let pre = turn
                .hook(HookEvent::PreToolUse {
                    tool_name: call.fn_name.clone(),
                    tool_input: call.fn_arguments.clone(),
                    tool_use_id: call.call_id.clone(),
                })
                .await?;
            turn.apply_common(&pre)?;
            turn.defer_context(pre.additional_contexts().to_vec());
            let blocking = pre.blocking_messages();
            let outcome = match pre.outcome {
                HookPointOutcome::PreToolUse(o) => o,
                _ => return Err(ErrorKind::Loop("invalid PreToolUse outcome".into()).into()),
            };
            if let Some(input) = outcome.updated_input {
                call.fn_arguments = input;
            }
            let permission = if !blocking.is_empty() {
                HookPermission::Deny {
                    reason: blocking.join("\n"),
                }
            } else {
                outcome.permission
            };
            let repeat = turn
                .repeated_tool
                .as_ref()
                .map_or(1, |(name, input, count)| {
                    if name == &call.fn_name && input == &call.fn_arguments {
                        count.saturating_add(1)
                    } else {
                        1
                    }
                });
            turn.repeated_tool = Some((call.fn_name.clone(), call.fn_arguments.clone(), repeat));
            let permission = if repeat >= 3 && !matches!(permission, HookPermission::Deny { .. }) {
                HookPermission::Ask {
                    reason: "Repeated identical tool call (doom_loop)".into(),
                }
            } else {
                permission
            };
            crate::permission::authorize(turn, &mut call, self, permission).await?;
            // Permission hooks may rewrite the input; count what will actually execute.
            if let Some((name, input, count)) = &mut turn.repeated_tool {
                if input != &call.fn_arguments {
                    *count = 1;
                }
                *name = call.fn_name.clone();
                *input = call.fn_arguments.clone();
            }
            turn.tc.check_control()?;
            turn.calls
                .iter_mut()
                .find(|record| record.call.call_id == call_id)
                .unwrap()
                .call = call.clone();
            self.run(turn, &call, &mut cleanup_deadline).await
        }
        .await;
        let aborted = result
            .as_ref()
            .err()
            .is_some_and(|e| matches!(e, YourAiError::Aborted(_)));
        let record = turn
            .calls
            .iter_mut()
            .find(|record| record.call.call_id == call_id)
            .unwrap();
        let (mut output, is_error) = match &record.state {
            CallState::Observed { output, is_error } => (output.clone(), *is_error),
            _ => match &result {
                Ok(value) => (value.clone(), false),
                Err(error) => (json!({"error":error.to_string()}), true),
            },
        };
        turn.observe_tool_result(&call, output.clone(), is_error);
        let event = match &result {
            Ok(value) => HookEvent::PostToolUse {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_response: value.clone(),
                tool_use_id: call.call_id.clone(),
            },
            Err(error) => HookEvent::PostToolUseFailure {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_use_id: call.call_id.clone(),
                error: error.to_string(),
                is_interrupt: Some(aborted),
            },
        };
        // Tool settling and cancellation reporting share one grace period.
        let kind = event.kind();
        let mut post_error = None;
        let post = if aborted {
            if let Some(runtime) = &turn.tc.snap.hooks {
                let invocation = turn.invocation(event);
                let deadline = cleanup_deadline.unwrap_or_else(|| {
                    tokio::time::Instant::now() + turn.config.tool_cleanup_timeout
                });
                if deadline <= tokio::time::Instant::now() {
                    None
                } else {
                    tokio::time::timeout_at(deadline, runtime.dispatch(&invocation))
                        .await
                        .ok()
                        .and_then(Result::ok)
                        .filter(|post| post.validate_for(kind).is_ok())
                }
            } else {
                None
            }
        } else {
            match turn.hook(event).await {
                Ok(post) => Some(post),
                Err(error) => {
                    post_error = Some(error);
                    None
                }
            }
        };
        let mut contexts = vec![];
        if let Some(post) = post {
            if let Err(error) = turn.apply_common(&post) {
                post_error = Some(error);
            }
            contexts = post.additional_contexts().to_vec();
            contexts.extend(post.blocking_messages());
            if let HookPointOutcome::PostToolUse(o) = post.outcome {
                if call.fn_name.starts_with("mcp__") {
                    if let Some(value) = o.updated_mcp_tool_output {
                        output = value;
                    }
                }
            }
        }
        turn.defer_context(contexts);
        turn.observe_tool_result(&call, output.clone(), is_error);
        if !aborted {
            if let Some(store) = turn.config.tool_output.clone() {
                output = turn
                    .wait_operation(store.bound(&output), turn.op_timeout(), "tool-output")
                    .await?;
            }
        }
        turn.observe_tool_result(&call, output.clone(), is_error);
        if aborted {
            return Err(result.expect_err("aborted tool must have an error"));
        } // cleanup commits the result with fresh time allowance
        let history = turn.history.clone();
        let row = turn
            .calls
            .iter()
            .find(|record| record.call.call_id == call_id)
            .unwrap()
            .result_record(&output);
        turn.wait_operation(history.append(vec![row]), turn.op_timeout(), "history")
            .await?;
        turn.calls
            .retain(|record| record.call.call_id != call.call_id);
        let settled = ExecutedTool {
            call: call.clone(),
            output: output.clone(),
            is_error,
        };
        turn.send(Out::ToolDone {
            id: call.call_id,
            name: call.fn_name,
            output,
            is_error,
        })?;
        if let Some(error) = post_error {
            return Err(error);
        }
        Ok(settled)
    }
    async fn run(
        &self,
        turn: &mut crate::execution::Turn<'_>,
        call: &ToolCall,
        cleanup_deadline: &mut Option<tokio::time::Instant>,
    ) -> Result<Value, YourAiError> {
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = Bridge { tx };
        let cancel = turn.tc.cancel.child_token();
        let _guard = cancel.clone().drop_guard();
        let session = turn.tc.info.options.session.clone();
        let providers = turn.tc.snap.clone();
        let tool_context = ToolContext {
            providers: Some(&providers),
            cwd: session.as_ref().map(|s| s.cwd.as_path()),
            call_id: call.call_id.clone(),
            emit: turn.tc.outbox,
            cancel: &cancel,
            security: turn.tc.snap.security.clone(),
            sandbox: turn.tc.snap.sandbox.clone(),
            interaction: Some(&bridge),
        };
        let future = self.backend.run(tool_context, call.fn_arguments.clone());
        tokio::pin!(future);
        let deadline = turn.deadline(
            turn.tc
                .info
                .options
                .limits
                .tool_timeout
                .or(turn.config.tool_timeout),
        );
        let mut request_ids = HashSet::new();
        loop {
            tokio::select! {
                biased;
                _ = turn.tc.cancel.cancelled() => return self.settle_cancelled_tool(turn, call, cleanup_deadline, &cancel, &mut future, AbortReason::Cancelled.into()).await,
                _ = turn.tc.outbox.closed() => return self.settle_cancelled_tool(turn, call, cleanup_deadline, &cancel, &mut future, AbortReason::Disconnected.into()).await,
                _ = crate::time::sleep_until(deadline) => return self.settle_cancelled_tool(turn, call, cleanup_deadline, &cancel, &mut future, turn.timeout_error("tool")).await,
                result = &mut future => return result,
                Some(mut pending) = rx.recv() => {
                    if pending.reply.is_closed() { continue; }
                    if pending.request.call_id != call.call_id || !request_ids.insert(pending.request.id.clone()) {
                        let _ = pending.reply.send(Err(ErrorKind::Config("invalid or duplicate interaction identity".into()).into()));
                        continue;
                    }
                    pending.request.deadline = match (pending.request.deadline, deadline) {
                        (Some(a), Some(b)) => Some(a.min(b)),
                        (a, b) => a.or(b),
                    };
                    let interaction_deadline = pending.request.deadline;
                    let response = tokio::select! {
                        biased;
                        result = &mut future => return result,
                        _ = pending.reply.closed() => continue,
                        _ = crate::time::sleep_until(interaction_deadline) => Err(turn.timeout_error("interaction")),
                        result = crate::interaction::elicit(turn, pending.request) => result,
                    };
                    let terminal = response.as_ref().err().is_some_and(|e| matches!(e, YourAiError::Aborted(_)));
                    if terminal { return self.settle_cancelled_tool(turn, call, cleanup_deadline, &cancel, &mut future, response.unwrap_err()).await; }
                    let _ = pending.reply.send(response);
                }
                input = turn.tc.inbox.recv(), if !turn.input_closed => match input {
                    Some(input) => turn.route(input), None => turn.input_closed = true,
                }
            }
        }
    }
    async fn settle_cancelled_tool(
        &self,
        turn: &mut crate::execution::Turn<'_>,
        call: &ToolCall,
        cleanup_deadline: &mut Option<tokio::time::Instant>,
        cancel: &tokio_util::sync::CancellationToken,
        future: impl std::future::Future<Output = Result<Value, YourAiError>>,
        cause: YourAiError,
    ) -> Result<Value, YourAiError> {
        cancel.cancel();
        let deadline = tokio::time::Instant::now() + turn.config.tool_cleanup_timeout;
        *cleanup_deadline = Some(deadline);
        if let Ok(result) = tokio::time::timeout_at(deadline, future).await {
            let (output, is_error) = match result {
                Ok(value) => (value, false),
                Err(error) => (json!({"error":error.to_string()}), true),
            };
            // Preserve any actual completion; cancellation still terminates the turn.
            turn.observe_tool_result(call, output, is_error);
        }
        Err(cause)
    }
}
#[derive(Debug, Clone)]
pub struct ExecutedTool {
    pub call: crate::chat::ToolCall,
    pub output: Value,
    pub is_error: bool,
}
/// Registration is the implementation side; lookup returns an opaque execution handle.
pub trait ToolRegistry: Send + Sync {
    fn register(&self, provider: Arc<dyn ToolProvider>);
    fn unregister(&self, name: &str);
    /// One atomic view: definitions and executable tools come from these frozen objects.
    fn snapshot(&self) -> Vec<Tool>;
    fn extend(&self, providers: Vec<Arc<dyn ToolProvider>>) {
        for provider in providers {
            self.register(provider);
        }
    }
    fn definitions(&self) -> Vec<ToolDefinition> {
        self.snapshot().iter().map(Tool::definition).collect()
    }
    fn resolve(&self, name: &str) -> Result<Tool, YourAiError> {
        self.snapshot()
            .into_iter()
            .find(|tool| tool.name() == name)
            .ok_or_else(|| {
                crate::ErrorKind::Tool {
                    name: name.into(),
                    message: "unknown tool".into(),
                }
                .into()
            })
    }
    fn has(&self, name: &str) -> bool {
        self.snapshot().iter().any(|tool| tool.name() == name)
    }
    fn count(&self) -> usize {
        self.snapshot().len()
    }
}

/// 便捷构造：工具执行失败 → 错误 ToolResponse（loop 用）
pub fn tool_error_response(call_id: &str, name: &str, err: &YourAiError) -> ToolResponse {
    ToolResponse::new(call_id, format!("ERROR: {err}")).with_fn_name(name.to_string())
}
