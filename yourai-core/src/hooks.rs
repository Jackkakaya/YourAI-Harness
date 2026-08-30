//! HookRegistry + HookHandler：事件驱动的拦截点。
//!
//! **多 handler 合并规则**（收敛结论，实现方必须遵守）：
//! 1. 按**注册顺序**串行分发（同注册调用的顺序即语义，不再是实现细节）
//! 2. 任一 handler 返回 `Block` → **短路**，立即返回该 Block
//! 3. `Modify` **链式传递**：前一个 handler 的 `new_input`
//!    成为下一个 handler 的 `tool_input`
//! 4. 全部 Continue → `Continue`；有 Modify 无 Block → 最后一个 Modify

use crate::error::YourAiError;
use crate::future::BoxFuture;
use serde_json::Value;
use std::sync::Arc;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum HookEventType {
    PreToolUse,
    PostToolUse,
    PreCompact,
    PostCompact,
    SessionStart,
    SessionEnd,
    TurnStart,
    TurnEnd,
    UserPromptSubmit,
}

#[derive(Debug, Clone)]
pub struct HookEvent {
    pub event_type: HookEventType,
    pub tool_name: Option<String>,
    pub tool_input: Option<Value>,
    pub tool_output: Option<Value>,
    pub user_message: Option<String>,
}

#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum HookOutcome {
    Continue,
    Block { reason: String },
    Modify { new_input: Value },
}

pub trait HookHandler: Send + Sync {
    fn id(&self) -> &str;
    fn event_types(&self) -> &[HookEventType];
    fn handle<'a>(
        &'a self,
        event: &'a HookEvent,
    ) -> BoxFuture<'a, Result<HookOutcome, YourAiError>>;
}

pub trait HookRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn HookHandler>);
    fn unregister(&self, id: &str);
    /// 按注册顺序串行分发（合并规则见模块文档）
    fn dispatch<'a>(&'a self, event: HookEvent) -> BoxFuture<'a, Result<HookOutcome, YourAiError>>;
    fn handler_ids(&self) -> Vec<String>;
}
