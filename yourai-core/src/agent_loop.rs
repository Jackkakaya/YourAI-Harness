//! AgentLoop：编排者接口（唯一可整体替换的"大脑"）。

use crate::context::TurnContext;
use crate::error::YourAiError;
use crate::future::BoxFuture;

/// 一次 turn 的产出。
///
/// - `pending`：Loop 已接收但尚未处理的消息；Core 在 Loop 返回后关闭 inbox，
///   再把尚未接收的消息追加进来，成功和失败路径都保留。
///   调用方负责用它们续 turn（followUp 机制）——loop 开不了新 turn，
///   turn 边界属于调用方。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct TurnOutput {
    pub text: String,
    pub usage: Option<yourai_protocol::Usage>,
    pub pending: Vec<yourai_protocol::In>,
}

impl TurnOutput {
    pub fn new(text: impl Into<String>) -> Self {
        TurnOutput {
            text: text.into(),
            usage: None,
            pending: Vec::new(),
        }
    }
}

/// 失败也携带部分文本、用量和未处理输入，不能因取消或错误丢失 follow-up。
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct TurnFailure {
    #[source]
    pub error: Box<YourAiError>,
    pub output: TurnOutput,
}

impl TurnFailure {
    pub fn new(error: impl Into<YourAiError>, output: TurnOutput) -> Self {
        Self {
            error: Box::new(error.into()),
            output,
        }
    }
}

impl From<YourAiError> for TurnFailure {
    fn from(error: YourAiError) -> Self {
        Self::new(error, TurnOutput::new(""))
    }
}

impl From<crate::error::ErrorKind> for TurnFailure {
    fn from(error: crate::error::ErrorKind) -> Self {
        Self::new(error, TurnOutput::new(""))
    }
}

impl From<crate::error::AbortReason> for TurnFailure {
    fn from(error: crate::error::AbortReason) -> Self {
        Self::new(error, TurnOutput::new(""))
    }
}

/// 运行成功与可恢复的失败报告。裸 `?` 只保留错误；已产生部分结果时，
/// Loop 应使用 TurnFailure::new 显式携带其本地累计状态。
pub type TurnResult = Result<TurnOutput, TurnFailure>;

/// 编排 turn 流程（模型调用 → 工具执行 → 重复直到完成）。
///
/// 最小签名，最大自由：只给 [`TurnContext`]（providers 快照 + inbox/outbox/cancel）。
/// 启动输入不是特殊参数——**它是 inbox 的第一条消息**，loop 只有一条读路径。
///
/// 消费契约：
/// - loop 是 inbox 的独占拉取消费者；step 边界 `try_recv`、
///   等待时 `recv().await`（务必与 `cancel` 一起 `select!`）；
///   已拉取但未处理的消息放入 [`TurnOutput::pending`]（失败则放入 TurnFailure）；
///   尚未拉取的消息由 Core 在正常返回后关闭并 drain，杜绝最后一次 drain 的竞态。
/// - outbox `send` 返回 `false` = 消费端已关闭，应尽快以
///   `Aborted(Disconnected)` 中止。
///
/// 生命周期：返回的 future 绑定 `&'a self` 与 `TurnContext<'a>`——
/// 有状态 loop 可在 async block 中借用自身字段，无需预先 clone。
pub trait AgentLoop: Send + Sync {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult>;
}
