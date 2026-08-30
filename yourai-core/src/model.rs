//! ModelProvider：LLM API 封装（直接使用 genai 类型，决策 5.1/5.6）。

use crate::chat::{ChatRequest, ChatResponse, ChatStreamResponse};
use crate::error::YourAiError;
use crate::future::BoxFuture;

pub trait ModelProvider: Send + Sync {
    /// 非流式调用
    fn complete(&self, req: ChatRequest) -> BoxFuture<'_, Result<ChatResponse, YourAiError>>;

    /// 流式调用（配合 [`ChatStreamEvent`](crate::chat::ChatStreamEvent) 消费；
    /// `StreamEnd` 的 captured_usage / captured_tool_calls 需在
    /// ChatOptions 中开启对应 capture）
    fn stream(&self, req: ChatRequest) -> BoxFuture<'_, Result<ChatStreamResponse, YourAiError>>;

    /// 当前模型标识（如 "deepseek-chat"）
    fn model_iden(&self) -> &str;
}
