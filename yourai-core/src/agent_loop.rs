//! AgentLoop：编排者接口（唯一可整体替换的"大脑"）。

use crate::context::TurnContext;
use crate::error::YourAiError;
use crate::future::BoxFuture;

/// 一次 turn 的产出。
///
/// - `pending`：退出前 inbox 里没消费的残留消息（决策 5.5 退出契约），
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

/// 编排 turn 流程（模型调用 → 工具执行 → 重复直到完成）。
///
/// 最小签名，最大自由：只给 [`TurnContext`]（providers 快照 + inbox/outbox/cancel）。
/// 启动输入不是特殊参数——**它是 inbox 的第一条消息**，loop 只有一条读路径。
///
/// 消费契约：
/// - loop 是 inbox 的独占拉取消费者；step 边界 `try_recv`、
///   等待时 `recv().await`（务必与 `cancel` 一起 `select!`）；
///   退出前必须再 drain 一次，残留放入 [`TurnOutput::pending`]。
/// - outbox `send` 返回 `false` = 消费端已关闭，应尽快以
///   `Aborted(Disconnected)` 中止。
///
/// 生命周期：返回的 future 绑定 `&'a self` 与 `TurnContext<'a>`——
/// 有状态 loop 可在 async block 中借用自身字段，无需预先 clone。
pub trait AgentLoop: Send + Sync {
    fn run_turn<'a>(
        &'a self,
        tc: TurnContext<'a>,
    ) -> BoxFuture<'a, Result<TurnOutput, YourAiError>>;
}
