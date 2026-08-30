//! ModelProvider：LLM API 封装（直接使用 genai 类型，决策 5.1/5.6）。
//!
//! 调用参数是 [`ModelRequest`]——`ChatRequest` + `ChatOptions` 一起走，
//! capture_usage / capture_tool_calls / capture_reasoning_content 等
//! 选项由 ContextManager 组装请求时带出，不再断链。

use crate::chat::{ChatOptions, ChatRequest, ChatResponse, ChatStreamResponse};
use crate::error::YourAiError;
use crate::future::BoxFuture;

/// 一次模型调用的完整参数。
#[derive(Debug, Clone)]
pub struct ModelRequest {
    pub request: ChatRequest,
    pub options: ChatOptions,
}

impl ModelRequest {
    pub fn new(request: ChatRequest, options: ChatOptions) -> Self {
        Self { request, options }
    }
}

pub trait ModelProvider: Send + Sync {
    /// 非流式调用
    fn complete<'a>(
        &'a self,
        req: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>>;

    /// 流式调用（配合 [`ChatStreamEvent`](crate::chat::ChatStreamEvent) 消费；
    /// `StreamEnd` 的 captured_usage / captured_tool_calls 需在
    /// ChatOptions 中开启对应 capture）
    fn stream<'a>(
        &'a self,
        req: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatStreamResponse, YourAiError>>;

    /// 当前模型标识（如 "deepseek-chat"）
    fn model_iden(&self) -> &str;
}
