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

/// 注入 ToolContext 的交互桥；不是全局 Context 中的新 Provider 插槽。
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
