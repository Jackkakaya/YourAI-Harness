//! ModelProvider：LLM API 封装（直接使用 genai 类型，决策 5.1/5.6）。
//!
//! 调用参数是 [`ModelRequest`]——`ChatRequest` + `ChatOptions` 一起走，
//! capture_usage / capture_tool_calls / capture_reasoning_content 等
//! 选项由 ContextManager 组装请求时带出，不再断链。

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

// region:    --- 公共模型执行入口（固定缝） ---

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

/// Infrastructure bridge between the Core entry and a runtime's execution.
/// Model providers implement [`ModelProvider`]; ordinary callers use [`exec`].
#[doc(hidden)]
pub trait ModelOperation: Send {
    fn exec_bound<'a>(
        &'a mut self,
        options: ModelOptions,
    ) -> BoxFuture<'a, Result<ModelOutput, YourAiError>>;
}

/// Fixed public model execution entry; always delegates to the supplied
/// framework operation, which owns the step lifecycle (request build,
/// streaming, timeouts, retry and StopFailure reporting).
pub fn exec<'a>(
    operation: &'a mut dyn ModelOperation,
    options: ModelOptions,
) -> BoxFuture<'a, Result<ModelOutput, YourAiError>> {
    operation.exec_bound(options)
}
// endregion: --- 公共模型执行入口（固定缝） ---
