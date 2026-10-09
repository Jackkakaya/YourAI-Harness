//! ModelProvider：LLM API 封装（直接使用 genai 类型，决策 5.1/5.6）。
//!
//! 调用参数是 [`ModelRequest`]——`ChatRequest` + `ChatOptions` 一起走，
//! capture_usage / capture_tool_calls / capture_reasoning_content 等
//! 选项由 ContextManager 组装请求时带出，不再断链。

mod budget;
pub use budget::{ModelLimits, ModelTokenBudget};

use crate::chat::ChatStreamEvent;
use crate::chat::{ChatOptions, ChatRequest, ChatResponse, ToolCall};
use crate::error::YourAiError;
use crate::future::BoxFuture;
use std::pin::Pin;

/// 可直接构造的统一事件流；genai 原始流由运行时适配器转换。
pub type ModelEventStream =
    Pin<Box<dyn futures_core::Stream<Item = Result<ChatStreamEvent, YourAiError>> + Send>>;

/// 只对尚未产生可见内容的失败尝试恢复。未知错误默认不可重试。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelRecovery {
    Fatal,
    Retry,
    Compact,
}

/// Provider-neutral failure semantics, independent of wire format and retry timing.
/// The selected model owns classification; core does not interpret vendor error codes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ModelErrorClass {
    QuotaExhausted,
    ContextOverflow,
    /// Opts into shared provider cooldown, using retry_after or the configured fallback.
    RateLimited,
    ServerError,
    Unclassified,
}

/// Defaults owned by the selected model, shared by every execution entry point.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTimeouts {
    pub headers: std::time::Duration,
    pub read: std::time::Duration,
}
/// 5 minutes matches long-thinking models' worst case before first byte.
const DEFAULT_MODEL_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);
impl Default for ModelTimeouts {
    fn default() -> Self {
        Self {
            headers: DEFAULT_MODEL_TIMEOUT,
            read: DEFAULT_MODEL_TIMEOUT,
        }
    }
}
impl ModelTimeouts {
    pub fn apply(self, options: &mut ChatOptions) {
        options.stream_header_timeout.get_or_insert(self.headers);
        options.stream_read_timeout.get_or_insert(self.read);
    }
}

/// 一次模型调用的完整参数。
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub request: ChatRequest,
    pub options: ChatOptions,
    /// Diagnostic metadata; never included in the model payload.
    pub source: &'static str,
    pub session_id: Option<String>,
    pub turn_id: Option<String>,
    /// One-based attempt for this model step, including retries.
    pub attempt: u32,
}

impl ModelRequest {
    pub fn with_context(mut self, source: &'static str, session_id: impl Into<String>) -> Self {
        self.source = source;
        self.session_id = Some(session_id.into());
        self
    }
    pub fn new(request: ChatRequest, options: ChatOptions) -> Self {
        Self {
            request,
            options,
            source: "main",
            session_id: None,
            turn_id: None,
            attempt: 1,
        }
    }
}

pub trait ModelProvider: Send + Sync {
    fn token_budget(&self) -> ModelTokenBudget {
        ModelTokenBudget::default()
    }
    fn timeouts(&self) -> ModelTimeouts {
        ModelTimeouts::default()
    }

    /// True when stream_header_timeout/stream_read_timeout are enforced around
    /// HTTP headers and raw body reads. The loop must not add event-idle timers.
    fn uses_transport_timeouts(&self) -> bool {
        false
    }

    /// Suggested wait after a failure. Does not consume an attempt.
    /// A RateLimited classification also applies this hint to shared admission;
    /// other classes use it only for local retries. Wrappers may merge shared waits.
    fn retry_after(&self, _error: &YourAiError) -> Option<std::time::Duration> {
        None
    }

    /// Model-specific media input budget. Unknown capabilities fail closed.
    fn media_tokens(&self, _part: &crate::chat::ContentPart) -> Result<u64, YourAiError> {
        Err(crate::ErrorKind::Config(
            "media budgeting/capability is not configured for this model".into(),
        )
        .into())
    }

    fn stream_events<'a>(
        &'a self,
        req: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>>;

    /// Provider-owned failure semantics. Unknown providers opt out of automatic
    /// recovery and shared cooldown until they supply a classification.
    /// Overriding recovery alone does not opt into shared cooldown.
    fn classify_error(&self, _error: &YourAiError) -> ModelErrorClass {
        ModelErrorClass::Unclassified
    }

    /// Adapters may override recovery independently of shared admission policy.
    fn recovery(&self, error: &YourAiError) -> ModelRecovery {
        match self.classify_error(error) {
            ModelErrorClass::RateLimited | ModelErrorClass::ServerError => ModelRecovery::Retry,
            ModelErrorClass::ContextOverflow => ModelRecovery::Compact,
            ModelErrorClass::QuotaExhausted | ModelErrorClass::Unclassified => ModelRecovery::Fatal,
        }
    }
    /// 非流式调用
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>>;

    fn model_iden(&self) -> &str;
}

// Execution values used by the core Turn.

/// Options for one model execution step.
#[derive(Debug, Clone)]
pub struct ModelOptions {
    pub tools_enabled: bool,
    pub prefill: Option<String>,
}
impl Default for ModelOptions {
    fn default() -> Self {
        Self {
            tools_enabled: true,
            prefill: None,
        }
    }
}

/// Result of one model execution step.
#[derive(Debug, Clone)]
pub struct ModelOutput {
    pub text: String,
    pub calls: Vec<ToolCall>,
}

use crate::prelude::*;
use futures_util::StreamExt;
use std::{collections::HashSet, sync::Arc};
/// The selected model owns the complete execution flow; raw transport stays private.
#[derive(Clone)]
pub struct Model {
    backend: Arc<dyn ModelProvider>,
}
impl Model {
    pub fn new(backend: Arc<dyn ModelProvider>) -> Self {
        Self { backend }
    }
    pub async fn exec(
        &self,
        turn: &mut crate::execution::Turn<'_>,
        options: ModelOptions,
    ) -> Result<ModelOutput, YourAiError> {
        turn.ensure_active()?;
        // Fixed public entry: the step lifecycle (request build, streaming,
        // timeouts, retry accounting, StopFailure reporting) lives in the
        // framework operation, never in the caller.
        if !turn.calls.is_empty() {
            return Err(ErrorKind::Loop(
                "resolve pending tool calls before requesting another model response".into(),
            )
            .into());
        }
        let options = &options;
        let model = self.backend.clone();
        let mut retries = 0;
        let mut overflow = 0;
        let mut force_compact = false;
        loop {
            turn.checkpoint().await?;
            if force_compact {
                turn.compact(CompactionTrigger::Overflow).await?;
                force_compact = false;
            }
            let attempt = self.run(turn, retries + 1, options).await;
            let calls = match attempt {
                Ok(value) => value,
                Err((error, visible)) => {
                    if matches!(error, YourAiError::Aborted(_)) {
                        return Err(error);
                    }
                    if !visible {
                        match model.recovery(&error) {
                            ModelRecovery::Compact
                                if overflow < turn.config.max_overflow_compactions =>
                            {
                                overflow += 1;
                                force_compact = true;
                                turn.notice(
                                    Level::Warning,
                                    "Context overflow; compacting before retry",
                                )?;
                                continue;
                            }
                            ModelRecovery::Retry if retries < turn.config.max_model_retries => {
                                let delay = crate::model_error::Backoff {
                                    initial: turn.config.retry_delay,
                                    max_without_headers: turn.config.retry_max_delay,
                                }
                                .delay(
                                    retries,
                                    &error,
                                    model.retry_after(&error),
                                    (uuid::Uuid::new_v4().as_u128() % 100) as u32,
                                );
                                retries += 1;
                                turn.send(Out::Retry {
                                    attempt: retries,
                                    max: turn.config.max_model_retries,
                                    reason: retry_cause(&error),
                                    wait_ms: delay.as_millis().min(u64::MAX as u128) as u64,
                                })?;
                                // checked_add: an absurd server retry-after must not panic here.
                                turn.wait_operation(
                                    async {
                                        tokio::time::sleep(delay).await;
                                        Ok(())
                                    },
                                    turn.config
                                        .operation_timeout
                                        .and_then(|op| delay.checked_add(op)),
                                    "retry",
                                )
                                .await?;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    let is_model_error =
                        matches!(&error, YourAiError::Error(ErrorKind::Model { .. }))
                            || matches!(
                                &error,
                                YourAiError::Error(ErrorKind::Provider { name, .. })
                                    if *name == "model"
                            );
                    if is_model_error {
                        self.stop_failure(turn, &error).await;
                    }
                    if let Some((status, _)) = crate::model_error::http_error(&error) {
                        turn.notice(Level::Warning, format!("HTTP {status}: stopped after {} attempt(s) for this model step; {} model call(s) in this turn", retries + 1, turn.model_calls))?;
                    }
                    return Err(error);
                }
            };
            let history = turn.history.clone();
            let record = turn
                .partial_message
                .clone()
                .expect("terminal response is owned");
            if record
                .message
                .content
                .texts()
                .iter()
                .any(|text| !text.trim().is_empty())
            {
                turn.repeated_tool = None;
            }
            turn.wait_operation(history.append(vec![record]), turn.op_timeout(), "history")
                .await?;
            turn.partial_message = None;
            turn.send(Out::Message {
                text: turn.output.text.clone(),
            })?;
            return Ok(ModelOutput {
                text: turn.output.text.clone(),
                calls,
            });
        }
    }
    async fn run(
        &self,
        turn: &mut crate::execution::Turn<'_>,
        attempt: u32,
        model_options: &ModelOptions,
    ) -> Result<Vec<ToolCall>, (YourAiError, bool)> {
        let mut visible = false;
        let result = async {
            let bindings = turn
                .tc
                .snap
                .tools
                .as_ref()
                .filter(|_| model_options.tools_enabled)
                .map(|registry| registry.snapshot())
                .unwrap_or_default();
            let tools: Vec<_> = bindings.iter().map(Tool::definition).collect();
            // Count the final outgoing prefill before admitting the request.
            // Workspace instruction updates are already part of persisted history.
            let suffix: Vec<_> = model_options.prefill.iter().map(|s| ChatMessage::assistant(s.clone())).collect();
            let mut prepared = turn.history.build_request(&tools, self.backend.as_ref(), &suffix)?;
            if prepared.maintenance_needed {
                if let Err(e) = turn.compact(CompactionTrigger::Threshold).await {
                    if matches!(e, YourAiError::Aborted(_)) || !prepared.fits() {
                        return Err(e);
                    }
                    // Do not continue with a stale view following an uncertain commit.
                    let history = turn.history.clone();
                    turn.wait_operation(history.restore(), turn.op_timeout(), "history").await?;
                    turn.notice(Level::Warning, format!("Automatic maintenance failed: {e}"))?;
                }
                prepared = turn.history.build_request(&tools, self.backend.as_ref(), &suffix)?;
            }
            if !prepared.fits() {
                return Err(ErrorKind::Loop(
                    "context exceeds configured input budget; no safe compaction available".into(),
                )
                .into());
            }
            let request = prepared.request;
            let observed_request = request.clone();
            let mut options = ChatOptions::default()
                .with_max_tokens(self.backend.token_budget().max_output_tokens())
                .with_capture_content(true)
                .with_capture_tool_calls(true)
                .with_capture_usage(true)
                .with_capture_reasoning_content(true);
            if !model_options.tools_enabled {
                options = options.with_tool_choice(ToolChoice::None);
            }
            let model = self.backend.clone();
            let model_timeout = turn.tc.info.options.limits.model_timeout;
            let header_timeout = model_timeout
                .or(options.stream_header_timeout)
                .unwrap_or(model.timeouts().headers);
            let chunk_timeout = model_timeout
                .or(options.stream_read_timeout)
                .unwrap_or(model.timeouts().read);
            turn.model_calls += 1;
            let mut request = ModelRequest::new(request, options);
            request.session_id = Some(turn.history.session_id().as_str().to_owned());
            request.turn_id = Some(turn.tc.info.id.to_string());
            request.attempt = attempt;
            let transport = model.uses_transport_timeouts();
            let total_deadline = turn.tc.info.options.limits.deadline;
            if transport {
                request.options.stream_header_timeout = Some(header_timeout);
                request.options.stream_read_timeout = Some(chunk_timeout);
            }
            let header_deadline = if transport {
                total_deadline
            } else {
                turn.deadline(Some(header_timeout))
            };
            let mut stream = turn
                .wait_until(model.stream_events(request), header_deadline, "model")
                .await?;
            let mut text = String::new();
            let mut reasoning = String::new();
            let mut saw_tool_chunk = false;
            loop {
                let deadline = if transport {
                    total_deadline
                } else {
                    turn.deadline(Some(chunk_timeout))
                };
                let next = match turn
                    .wait_until(async { stream.next().await.transpose() }, deadline, "model")
                    .await
                {
                    Ok(next) => next,
                    Err(e) => return Err(e),
                };
                let Some(event) = next else {
                    return Err(ErrorKind::Provider {
                        name: "model",
                        message: "stream ended without terminal event".into(),
                    }
                    .into());
                };
                match event {
                    ChatStreamEvent::Start | ChatStreamEvent::Heartbeat => {}
                    ChatStreamEvent::Chunk(chunk) => {
                        if !chunk.content.is_empty() {
                            text.push_str(&chunk.content);
                            visible = true;
                            keep_partial(turn, &text, &reasoning);
                            turn.send(Out::Chunk {
                                text: chunk.content,
                            })?;
                        }
                    }
                    ChatStreamEvent::ReasoningChunk(chunk) => {
                        if !chunk.content.is_empty() {
                            visible = true;
                            reasoning.push_str(&chunk.content);
                            keep_partial(turn, &text, &reasoning);
                            turn.send(Out::Reasoning {
                                text: chunk.content,
                            })?;
                        }
                    }
                    ChatStreamEvent::ToolCallChunk(_) => {
                        saw_tool_chunk = true;
                    }
                    ChatStreamEvent::ThoughtSignatureChunk(_) => {} // Preserved in captured content.
                    ChatStreamEvent::End(end) => {
                        // Nothing after End can be retried: accounting/persistence is not a model retry.
                        visible = true;
                        if let Some(content) = &end.captured_content {
                            text = content.texts().join("");
                            turn.output.text = text.clone();
                        }
                        keep_partial(turn, &text, &reasoning);
                        let calls: Vec<_> = end
                            .captured_tool_calls()
                            .unwrap_or_default()
                            .into_iter()
                            .cloned()
                            .collect();
                        let validation = (|| -> Result<(), YourAiError> {
                            if matches!(
                                end.captured_stop_reason,
                                Some(StopReason::MaxTokens(_) | StopReason::ContentFilter(_))
                            ) {
                                return Err(ErrorKind::Provider {
                                    name: "model",
                                    message:
                                        "model response truncated or filtered; tools will not execute"
                                            .into(),
                                }
                                .into());
                            }
                            if saw_tool_chunk && calls.is_empty() {
                                return Err(
                                    ErrorKind::Loop("stream lost captured tool calls".into()).into()
                                );
                            }
                            let mut ids = HashSet::new();
                            for call in &calls {
                                if call.call_id.is_empty()
                                    || call.fn_name.is_empty()
                                    || turn.call_ids.contains(&call.call_id)
                                    || turn.history.contains_tool_call(&call.call_id)
                                    || !ids.insert(call.call_id.clone())
                                {
                                    return Err(ErrorKind::Loop(
                                        "missing or duplicate tool-call identity".into(),
                                    )
                                    .into());
                                }
                            }
                            Ok(())
                        })();
                        let content = if validation.is_ok() {
                            end.captured_content.unwrap_or_else(|| MessageContent::from_text(text.clone()))
                        } else {
                            // Incomplete or invalid calls are never published or executed.
                            MessageContent::from_text(text.clone())
                        };
                        turn.output.text = content.texts().join("");
                        let message = ChatMessage::assistant(content).with_reasoning_content(
                            end.captured_reasoning_content
                                .or_else(|| (!reasoning.is_empty()).then_some(reasoning)),
                        );
                        // Own the entire terminal response before accounting or storage can fail.
                        let observation = end.captured_usage.as_ref().and_then(|u| {
                            u.prompt_tokens.filter(|n| *n >= 0).map(|n| RequestObservation {
                                model: model.model_iden().into(),
                                request: observed_request.clone(),
                                input_tokens: n as u64,
                            })
                        });
                        let mut record = turn.partial_message.clone()
                            .unwrap_or_else(|| StoredMessage::new(message.clone()));
                        record.message = message.clone();
                        record.model_response = validation.is_ok();
                        record.request_observation = observation;
                        turn.partial_message = Some(record);
                        if validation.is_ok() { turn.register_calls(calls.clone(), &bindings); }
                        if let Some(u) = &end.captured_usage {
                            turn.record_usage(model_usage(u)).await?;
                        }
                        if let Some(tracker) = turn.tc.snap.usage.clone() {
                            let session_id = turn.history.session_id().clone();
                            let event = UsageEvent::new(
                                Some(model.model_iden().into()), "main",
                                end.captured_usage.clone().unwrap_or_default(),
                            );
                            match turn.wait_operation(async { Ok(tracker.record_response(&session_id, &event).await) }, turn.op_timeout(), "usage").await {
                                Ok(Some(warning)) => turn.notice(Level::Warning, warning)?,
                                Ok(None) => {},
                                Err(e @ YourAiError::Aborted(_)) => return Err(e),
                                Err(e) => turn.notice(Level::Warning, format!("Usage accounting incomplete: {e}"))?,
                            }
                        }
                        validation?;
                        return Ok(calls);
                    }
                }
            }
        }
        .await;
        result.map_err(|error| (error, visible))
    }

    async fn stop_failure(&self, turn: &mut crate::execution::Turn<'_>, cause: &YourAiError) {
        // A reporting hook cannot overwrite the model error or block cleanup forever.
        let event = HookEvent::StopFailure {
            error: "model_error".into(),
            error_details: Some(cause.to_string()),
            last_assistant_message: Some(turn.output.text.clone()),
        };
        if let Ok(result) = turn.hook(event).await {
            let _ = turn.apply_common(&result);
        }
    }
}
/// Safe, structured error context for retry notices; never print request payloads.
pub(crate) fn retry_cause(error: &YourAiError) -> String {
    let Some((status, body)) = crate::model_error::http_error(error) else {
        return "model provider error".into();
    };
    let mut reason = format!("HTTP {status}");
    if let Ok(value) = serde_json::from_str::<serde_json::Value>(body) {
        for field in ["code", "dimension"] {
            if let Some(value) = value
                .get("error")
                .and_then(|e| e.get(field))
                .and_then(|v| v.as_str())
            {
                if !value.is_empty()
                    && value.len() <= 64
                    && value
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
                {
                    reason.push_str(&format!(", {field}={value}"));
                }
            }
        }
    }
    reason
}

fn model_usage(u: &GenaiUsage) -> Usage {
    let input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
    let output = u.completion_tokens.unwrap_or(0).max(0) as u64;
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: u
            .total_tokens
            .map(|n| n.max(0) as u64)
            .unwrap_or(input + output),
    }
}

fn keep_partial(turn: &mut crate::execution::Turn<'_>, text: &str, reasoning: &str) {
    turn.output.text = text.to_owned();
    if !text.is_empty() || !reasoning.is_empty() {
        let message = ChatMessage::assistant(text.to_owned())
            .with_reasoning_content((!reasoning.is_empty()).then(|| reasoning.to_owned()));
        if let Some(record) = &mut turn.partial_message {
            record.message = message;
        } else {
            turn.partial_message = Some(StoredMessage::new(message));
        }
    }
}
