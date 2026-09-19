//! ModelProvider：LLM API 封装（直接使用 genai 类型，决策 5.1/5.6）。
//!
//! 调用参数是 [`ModelRequest`]——`ChatRequest` + `ChatOptions` 一起走，
//! capture_usage / capture_tool_calls / capture_reasoning_content 等
//! 选项由 ContextManager 组装请求时带出，不再断链。

use crate::chat::ChatStreamEvent;
use crate::chat::{ChatOptions, ChatRequest, ChatResponse};
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
    /// Shared provider cooldown remaining after a failure. Does not consume an attempt.
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

    /// 适配器可以按结构化服务端错误覆盖分类；不以任意错误文本猜测溢出。
    fn recovery(&self, error: &YourAiError) -> ModelRecovery {
        if let Some((status, body)) = error.model_http_error() {
            let code = serde_json::from_str::<serde_json::Value>(body)
                .ok()
                .and_then(|v| {
                    v.pointer("/error/code")
                        .and_then(|v| v.as_str())
                        .map(str::to_owned)
                });
            if matches!(
                code.as_deref(),
                Some("insufficient_quota" | "billing_hard_limit_reached")
            ) {
                return ModelRecovery::Fatal;
            }
            if code.as_deref() == Some("context_length_exceeded") {
                return ModelRecovery::Compact;
            }
            if status == 429 || (500..600).contains(&status) {
                return ModelRecovery::Retry;
            }
        }
        ModelRecovery::Fatal
    }
    /// 非流式调用
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>>;

    fn model_iden(&self) -> &str;
}
