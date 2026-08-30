//! SecurityProvider：审批与权限决策（纯决策函数，不知道 UI 存在）。
//!
//! 返回 [`ApprovalDecision::Ask`] 时，审批的交互中介是 loop：
//! loop 发 `Out::Ask`、等 `In::Reply`（select! cancel）。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use serde_json::Value;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum ApprovalDecision {
    Allow,
    Deny,
    /// 交给人决定——交互由 loop 中介
    Ask,
}

/// 一次待审动作的上下文
#[derive(Debug, Clone)]
pub struct SecurityContext {
    pub action: String,
    pub input: Value,
    pub is_destructive: bool,
    pub is_network: bool,
}

pub trait SecurityProvider: Send + Sync {
    fn check_tool_call(&self, ctx: &SecurityContext)
        -> BoxFuture<'_, Result<ApprovalDecision, YourAiError>>;
    fn check_command(&self, command: &str) -> BoxFuture<'_, Result<ApprovalDecision, YourAiError>>;
    fn check_file_access(
        &self,
        path: &str,
        write: bool,
    ) -> BoxFuture<'_, Result<ApprovalDecision, YourAiError>>;
}
