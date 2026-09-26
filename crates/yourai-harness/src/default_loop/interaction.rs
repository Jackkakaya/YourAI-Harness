use super::State;
use serde_json::{json, Value};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;

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

impl State<'_> {
    pub(crate) async fn ask(
        &mut self,
        id: String,
        payload: Value,
        timeout: Option<Duration>,
    ) -> Result<Value, YourAiError> {
        self.tc.check_control()?;
        // Discard pre-sent replies before publishing a new request.
        self.drain();
        self.send(Out::Ask {
            id: id.clone(),
            payload,
        })?;
        let deadline = self.deadline(timeout);
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = async { match deadline {
                    Some(d) => tokio::time::sleep_until(d.into()).await,
                    None => std::future::pending().await,
                }} => return Err(self.timeout_error("approval")),
                input = self.tc.inbox.recv() => match input {
                    Some(In::Reply { id: reply_id, payload }) if reply_id == id => return Ok(payload),
                    Some(input) => self.route(input),
                    None => { self.input_closed = true; return Err(AbortReason::Disconnected.into()); }
                }
            }
        }
    }
    pub(crate) async fn service_interaction(
        &mut self,
        request: InteractionRequest,
    ) -> Result<Value, YourAiError> {
        let timeout = {
            let from_request = request
                .deadline
                .map(|d| d.saturating_duration_since(Instant::now()));
            let from_limits = self.tc.info.options.limits.approval_timeout;
            let from_config = self.config.approval_timeout;
            [from_request, from_limits, from_config]
                .into_iter()
                .flatten()
                .min()
        };
        let mut payload = request.payload.clone();
        if let Some(object) = payload.as_object_mut() {
            object.insert("call_id".into(), json!(request.call_id));
        } else {
            payload = json!({"call_id": request.call_id, "question": payload});
        }
        match &request.kind {
            InteractionKind::Question => self.ask(request.id, payload, timeout).await,
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
                let before = self
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
                self.apply_common(&before)?;
                let mut answer = if !before.common.blocking_errors.is_empty() {
                    json!({"action":"decline"})
                } else if let HookPointOutcome::Elicitation(o) = before.outcome {
                    if let Some(action) = o.action {
                        json!({"action":action,"content":o.content})
                    } else {
                        self.ask(request.id, payload, timeout).await?
                    }
                } else {
                    return Err(ErrorKind::Loop("invalid Elicitation outcome".into()).into());
                };
                validate_elicitation(&answer, schema.as_ref())?;
                let after = self
                    .hook(HookEvent::ElicitationResult {
                        mcp_server_name: server_name.clone(),
                        elicitation_id: elicitation_id.clone(),
                        mode,
                        action: answer["action"].as_str().unwrap_or_default().into(),
                        content: answer.get("content").cloned(),
                    })
                    .await?;
                self.apply_common(&after)?;
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
            _ => Err(ErrorKind::Config("unsupported interaction kind".into()).into()),
        }
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
