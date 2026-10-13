//! 工具向 Loop 请求交互的接口。只有 Loop 消费 In::Reply。
//!
//! 默认 Loop 可用内部请求通道 + oneshot 回复实现本接口；core 不规定调度器。

use std::time::Instant;

use serde_json::Value;
use tokio_util::sync::CancellationToken;

use crate::{BoxFuture, YourAiError};

/// Loop 据此选择普通提问或 MCP 专属 Elicitation / ElicitationResult Hook。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum InteractionKind {
    Question,
    McpElicitation {
        server_name: String,
        elicitation_id: Option<String>,
    },
}

/// 一次用户交互。id 独立于 call_id，允许同一工具多次提问。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct InteractionRequest {
    pub id: String,
    pub call_id: String,
    pub kind: InteractionKind,
    /// 表单、问题或 MCP mode/url/schema；不能借此放宽 Security 的硬限制。
    pub payload: Value,
    pub deadline: Option<Instant>,
}

impl InteractionRequest {
    pub fn new(call_id: impl Into<String>, kind: InteractionKind, payload: Value) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            call_id: call_id.into(),
            kind,
            payload,
            deadline: None,
        }
    }
}

/// 注入 ToolContext 的交互桥；不是全局 Providers 中的新 Provider 插槽。
///
/// 实现必须：
/// - 将请求交给 Loop，等待 Loop 发 Ask 并匹配 Reply，不自行读取 inbox；
/// - 处理请求与 Turn 的截止时间、取消、断开和无效回复；
/// - 对 MCP 请求应用对应 Hook 并校验最终 action/content；
/// - 等待被丢弃或取消时注销待回复请求，迟到回复不得交给另一请求；
/// - 非交互运行立即报 Config，不得等待永远不会到来的回复。
///
/// Loop 等待工具执行时必须同时服务该接口背后的请求通道，避免循环等待。
pub trait ToolInteraction: Send + Sync {
    fn request<'a>(
        &'a self,
        request: InteractionRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, YourAiError>>;
}

use crate::prelude::*;
use serde_json::json;

use tokio::sync::{mpsc, oneshot};
pub(crate) struct PendingInteraction {
    pub request: InteractionRequest,
    pub reply: oneshot::Sender<Result<Value, YourAiError>>,
}
pub(crate) struct Bridge {
    pub tx: mpsc::UnboundedSender<PendingInteraction>,
}
impl ToolInteraction for Bridge {
    fn request<'a>(
        &'a self,
        request: InteractionRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let (tx, rx) = oneshot::channel();
            self.tx
                .send(PendingInteraction { request, reply: tx })
                .map_err(|_| AbortReason::Disconnected)?;
            tokio::select! {
                biased;
                _ = cancel.cancelled() => Err(AbortReason::Cancelled.into()),
                result = rx => result.map_err(|_| YourAiError::from(AbortReason::Disconnected))?,
            }
        })
    }
}

pub(crate) fn validate_schema(schema: &Value, value: &Value) -> Result<(), YourAiError> {
    // Remote reference retrieval is disabled by Cargo features.
    let validator = jsonschema::validator_for(schema)
        .map_err(|e| ErrorKind::Config(format!("invalid tool/interaction schema: {e}")))?;
    validator.validate(value).map_err(|e| {
        ErrorKind::Tool {
            name: "input_validation".into(),
            message: e.to_string(),
        }
        .into()
    })
}

pub async fn elicit(
    turn: &mut crate::execution::Turn<'_>,
    request: InteractionRequest,
) -> Result<Value, YourAiError> {
    turn.ensure_active()?;
    let timeout = [
        turn.tc.info.options.limits.approval_timeout,
        turn.config.approval_timeout,
    ]
    .into_iter()
    .flatten()
    .min();
    let total_deadline = turn.tc.info.options.limits.deadline;
    let deadline = [request.deadline, turn.deadline(timeout)]
        .into_iter()
        .flatten()
        .min();
    let cancel = turn.tc.cancel.clone();
    let outbox = turn.tc.outbox;
    let timeout_error = || {
        if total_deadline.is_some_and(|d| Instant::now() >= d) {
            YourAiError::from(AbortReason::DeadlineExceeded)
        } else {
            YourAiError::from(ErrorKind::Provider {
                name: "approval",
                message: "operation timed out".into(),
            })
        }
    };
    // One absolute bound includes both hooks, the reply and final validation.
    let operation = async {
        let mut payload = request.payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("call_id".into(), json!(request.call_id));
        } else {
            payload = json!({"call_id": request.call_id, "question": payload});
        }
        match &request.kind {
            InteractionKind::Question => {
                turn.ask(
                    request.id,
                    payload,
                    deadline.map(|d| d.saturating_duration_since(Instant::now())),
                )
                .await
            }
            InteractionKind::McpElicitation {
                server_name,
                elicitation_id,
            } => {
                let mode = request
                    .payload
                    .get("mode")
                    .and_then(Value::as_str)
                    .map(str::to_owned);
                let schema = request
                    .payload
                    .get("requested_schema")
                    .or_else(|| request.payload.get("requestedSchema"))
                    .cloned();
                let before = turn
                    .hook(HookEvent::Elicitation {
                        mcp_server_name: server_name.clone(),
                        elicitation_id: elicitation_id.clone(),
                        mode: mode.clone(),
                        message: request
                            .payload
                            .get("message")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .into(),
                        url: request
                            .payload
                            .get("url")
                            .and_then(Value::as_str)
                            .map(str::to_owned),
                        requested_schema: schema.clone(),
                    })
                    .await?;
                turn.apply_common(&before)?;
                let mut answer = if !before.common.blocking_errors.is_empty() {
                    json!({"action":"decline"})
                } else if let HookPointOutcome::Elicitation(o) = before.outcome {
                    if let Some(action) = o.action {
                        json!({"action":action,"content":o.content})
                    } else {
                        turn.ask(
                            request.id,
                            payload,
                            deadline.map(|d| d.saturating_duration_since(Instant::now())),
                        )
                        .await?
                    }
                } else {
                    return Err(ErrorKind::Loop("invalid Elicitation outcome".into()).into());
                };
                validate_elicitation(&answer, schema.as_ref())?;
                let after = turn
                    .hook(HookEvent::ElicitationResult {
                        mcp_server_name: server_name.clone(),
                        elicitation_id: elicitation_id.clone(),
                        mode,
                        action: answer["action"].as_str().unwrap_or_default().into(),
                        content: answer.get("content").cloned(),
                    })
                    .await?;
                turn.apply_common(&after)?;
                if !after.common.blocking_errors.is_empty() {
                    answer = json!({"action":"decline"});
                } else if let HookPointOutcome::ElicitationResult(o) = after.outcome {
                    if let Some(action) = o.action {
                        answer["action"] = json!(action);
                    }
                    if let Some(content) = o.content {
                        answer["content"] = content;
                    }
                } else {
                    return Err(ErrorKind::Loop("invalid ElicitationResult outcome".into()).into());
                }
                validate_elicitation(&answer, schema.as_ref())?;
                Ok(answer)
            }
        }
    };
    tokio::select! { biased;
        _ = cancel.cancelled() => Err(AbortReason::Cancelled.into()),
        _ = outbox.closed() => Err(AbortReason::Disconnected.into()),
        _ = crate::time::sleep_until(deadline) => Err(timeout_error()),
        result = operation => {
            // Synchronous schema validation cannot yield to the timer.
            if deadline.is_some_and(|d| Instant::now() >= d) {
                Err(timeout_error())
            } else {
                result
            }
        },
    }
}
fn validate_elicitation(answer: &Value, schema: Option<&Value>) -> Result<(), YourAiError> {
    match answer.get("action").and_then(Value::as_str) {
        Some("accept") => {
            if let Some(schema) = schema {
                validate_schema(schema, answer.get("content").unwrap_or(&Value::Null))?;
            } else if answer
                .get("content")
                .is_some_and(|v| !v.is_null() && !v.is_object())
            {
                return Err(
                    ErrorKind::Config("elicitation content must be an object".into()).into(),
                );
            }
            Ok(())
        }
        Some("decline" | "cancel") => Ok(()),
        _ => Err(ErrorKind::Config("invalid elicitation action".into()).into()),
    }
}
