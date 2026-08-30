//! ContextManager：对话历史管理（不是 Context 容器！）。
//!
//! 决策 5.4：实现持有 session_id、自 load/save；SessionManager 只管元数据。
//! 工具结果以 genai [`ToolResponse`] 形态进入历史——call_id 贯穿
//! （并行工具调用靠 call_id 关联，决策收敛修订）。

use crate::chat::{ChatMessage, ChatOptions, ChatRequest, Tool, ToolResponse};
use crate::error::YourAiError;
use crate::future::BoxFuture;
use yourai_protocol::Usage;

/// 管理发给模型的消息序列（历史、窗口、请求组装）。
#[allow(clippy::type_complexity)]
pub trait ContextManager: Send + Sync {
    /// 当前历史（已按窗口策略裁剪后的可发送序列）
    fn messages(&self) -> Vec<ChatMessage>;

    fn add_user_message<'a>(&'a self, text: &'a str) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn add_assistant_message<'a>(&'a self, text: &'a str)
        -> BoxFuture<'a, Result<(), YourAiError>>;

    /// 记录一次工具调用结果（call_id 与发起的 ToolCall 关联）
    fn add_tool_result<'a>(
        &'a self,
        response: ToolResponse,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;

    fn add_message<'a>(&'a self, message: ChatMessage) -> BoxFuture<'a, Result<(), YourAiError>>;

    /// 组装请求（无工具）
    fn build_request(&self) -> ChatRequest;
    /// 组装请求（带工具 schema 与可选 system prompt）
    fn build_request_with(&self, tools: &[Tool], system: Option<&str>) -> ChatRequest;

    /// 压缩历史（大上下文时由 loop 触发）
    fn compact<'a>(&'a self) -> BoxFuture<'a, Result<(), YourAiError>>;

    /// 粗略 token 计数
    fn token_count(&self) -> u64;

    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), YourAiError>>;

    /// 记录一次 LLM 用量到历史统计（记账主通道是 UsageTracker）
    fn record_usage<'a>(&'a self, usage: &'a Usage) -> BoxFuture<'a, Result<(), YourAiError>>;

    /// 组装请求时的默认 ChatOptions（capture 开关等，随 ModelRequest 下发）
    fn default_options(&self) -> ChatOptions {
        ChatOptions::default()
    }
}
