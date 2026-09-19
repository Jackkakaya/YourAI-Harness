//! SecurityProvider：审批与权限决策（纯决策函数，不知道 UI 存在）。
//!
//! **两层审批**（架构收敛结论）：
//! - 第一层：[`SecurityProvider::check_tool_call`]——loop 在调度工具前统一问；
//!   返回 [`ApprovalDecision::Ask`] 时，交互中介是 loop：
//!   loop 发 `Out::Ask`、等 `In::Reply`（select! cancel）
//! - 第二层：`check_command` / `check_file_access`——只有具体工具
//!   （shell/fs）知道命令与路径，由工具经 ToolContext 注入的能力调用；
//!   这一层只做不可交互的策略强制，返回 [`PolicyDecision`]（Allow/Deny）。
//!   权限审批在第一层完成。工具的普通提问/MCP elicitation 可使用独立的
//!   ToolInteraction 桥，但不能通过交互绕过第二层硬限制。

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

/// 工具内部的强制策略结果。
///
/// 第二层检查发生在 loop 已经开始等待工具执行之后，没有 inbox 消费权，
/// 权限检查本身不能发起 Ask/Reply；需要审批的判断由第一层完成。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum PolicyDecision {
    Allow,
    Deny,
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
    /// Apply validated policy updates atomically. Unsupported providers fail explicitly.
    fn update_permissions<'a>(
        &'a self,
        _updates: &'a [Value],
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async {
            Err(crate::ErrorKind::Config("permission updates unsupported".into()).into())
        })
    }
    /// 第一层：工具调用级审批（loop 调用）
    fn check_tool_call<'a>(
        &'a self,
        ctx: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>>;

    /// 第二层：命令级审批（shell 类工具自调）
    fn check_command<'a>(
        &'a self,
        command: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>>;

    /// 第二层：文件访问审批（fs 类工具自调）
    fn check_file_access<'a>(
        &'a self,
        path: &'a str,
        write: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>>;
}
