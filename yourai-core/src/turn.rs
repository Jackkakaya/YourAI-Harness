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

/// Loop 接受的执行约束。None 表示未指定，0 表示不允许相应调用。
///
/// Core 传递限制，Loop 负责在实际操作边界执行它们；这不是强制终止器。
/// 模型重试计入 model_calls，工具计数在进入实际执行前增加。
/// 压缩、模型 Hook 等额外模型调用需要实现方接入同一记账路径。
/// 不提供“全局费用预算”，避免在尚未统一记账时作出错误保证。
#[derive(Debug, Clone, Default)]
#[non_exhaustive]
pub struct TurnLimits {
    pub max_model_calls: Option<u32>,
    pub max_tool_calls: Option<u32>,
    /// 绝对截止时间；重试、审批、压缩不得重新开始总计时。
    pub deadline: Option<Instant>,
    pub model_timeout: Option<Duration>,
    pub tool_timeout: Option<Duration>,
    pub approval_timeout: Option<Duration>,
    pub hook_timeout: Option<Duration>,
}

/// 可解释的额度耗尽原因。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum TurnLimit {
    ModelCalls,
    ToolCalls,
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
