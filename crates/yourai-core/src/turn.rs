//! Turn 的身份、会话绑定与执行限制；不包含队列或业务调度策略。

use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::session_runtime::SessionContext;

/// 一次执行的身份，由 Core 在启动时生成。
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TurnId(String);

impl TurnId {
    pub fn new() -> Self {
        Self(uuid::Uuid::new_v4().to_string())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl Default for TurnId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for TurnId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// Loop 接受的执行约束。None 表示未指定。
///
/// `steps` 与 OpenCode 的 agent `steps` 一致：它计算 agentic iteration，
/// 重试和压缩内部模型调用不额外消耗 step；达到最后一步时应禁用工具并强制文本收尾。
/// 未指定时 Loop 仍会施加 `MAX_AGENT_STEPS`（1000）硬上限，对齐 OpenCode
/// `streamText` 的 `stopWhen`（`steps.length >= 1000`）安全网。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TurnLimits {
    pub steps: Option<u32>,
    /// 绝对截止时间；重试、审批、压缩不得重新开始总计时。
    pub deadline: Option<Instant>,
    pub model_timeout: Option<Duration>,
    pub tool_timeout: Option<Duration>,
    pub approval_timeout: Option<Duration>,
    pub hook_timeout: Option<Duration>,
}

/// 宿主交给 Agent 的单次执行参数。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TurnOptions {
    /// None 允许独立的无会话 Loop；会话宿主应始终提供绑定信息。
    pub session: Option<Arc<SessionContext>>,
    pub limits: TurnLimits,
    pub events: Option<Arc<crate::runtime_event::RuntimeEvents>>,
}

/// 启动时确定的元数据，Loop 不从可变的全局环境猜测会话身份。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TurnInfo {
    pub id: TurnId,
    pub started_at: Instant,
    pub options: TurnOptions,
}

impl TurnInfo {
    pub(crate) fn new(options: TurnOptions) -> Self {
        Self {
            id: TurnId::new(),
            started_at: Instant::now(),
            options,
        }
    }
}
