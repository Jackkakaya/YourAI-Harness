//! HookRegistry + HookHandler：事件驱动的拦截点。

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
    fn handle(&self, event: &HookEvent) -> BoxFuture<'_, Result<HookOutcome, YourAiError>>;
}

pub trait HookRegistry: Send + Sync {
    fn register(&self, handler: Arc<dyn HookHandler>);
    fn unregister(&self, id: &str);
    /// 分发给所有订阅该事件类型的 handler（分发顺序为实现细节）
    fn dispatch(&self, event: HookEvent) -> BoxFuture<'_, Result<HookOutcome, YourAiError>>;
    fn handler_ids(&self) -> Vec<String>;
}
