//! 会话宿主契约（图 1），不提供默认宿主实现，也不是 Provider 插槽。
//!
//! SessionManager 管元数据；本接口管理活着的会话。创建/恢复由具体实现的
//! 构造入口完成：绑定历史、装配 Agent、触发 SessionStart 后才对外提供对象。
//!
//! ```
//! use yourai_core::prelude::*;
//!
//! fn enqueue(runtime: &dyn SessionRuntime, text: &str) -> Result<(), InputRejected> {
//!     runtime.submit(In::user_text(text))
//! }
//! ```

use std::path::PathBuf;
use std::time::Duration;

use crate::protocol::In;
use tokio_util::sync::CancellationToken;

use crate::agent_loop::TurnResult;
use crate::compaction::{CompactionRequest, CompactionResult};
use crate::session::SessionId;
use crate::turn::{TurnId, TurnLimits};
use crate::{BoxFuture, OutSink, YourAiError};

/// 会话运行环境。每次启动拷贝到 TurnOptions，不在运行中修改旧快照。
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct SessionContext {
    pub id: SessionId,
    pub cwd: PathBuf,
    pub transcript_path: Option<PathBuf>,
    /// Host-loaded instructions, copied into each request environment.
    pub instructions: std::collections::BTreeMap<PathBuf, String>,
}

impl SessionContext {
    pub fn new(id: SessionId, cwd: impl Into<PathBuf>) -> Self {
        Self {
            id,
            cwd: cwd.into(),
            transcript_path: None,
            instructions: Default::default(),
        }
    }
}

/// 仅用于展示；submit/run_next/compact/close 必须各自原子地检查真实状态。
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub enum SessionStatus {
    Idle,
    Running { turn_id: TurnId },
    Compacting,
    Closing,
    Closed,
}

/// 输入未被接纳时归还所有权。宿主不得既返回本错误又把输入留在队列。
#[derive(Debug, thiserror::Error)]
#[error("session input rejected: {reason}")]
pub struct InputRejected {
    pub input: In,
    pub reason: String,
}

/// 一个已结束 Turn 的报告；执行故障也在 result 中，而不是 run_next 的外层 Err。
#[derive(Debug)]
pub struct SessionTurn {
    pub turn_id: TurnId,
    pub result: TurnResult,
}

/// 对外统一的会话控制接口，适用于 TUI 和 Web 适配器。
///
/// 外层驱动通过 submit 接纳输入、run_next 驱动一个 Turn，可据策略再次调用
/// run_next 处理 follow-up。每次调用的 outbox 只对应一个 Turn。
/// 本接口不要求新的常驻 actor、事件总线或执行线程。
pub trait SessionRuntime: Send + Sync {
    fn context(&self) -> SessionContext;
    fn status(&self) -> SessionStatus;

    /// 空闲时排队用户输入；运行中按 InputMode 路由追加输入。
    /// Reply 只能投递到当前有效交互，不能排队为下一个 Turn 的启动输入。
    /// 在关闭/拒绝/交接失败时归还输入，由调用方决定是否重投。
    fn submit(&self, input: In) -> Result<(), InputRejected>;

    /// 消费下一条排队输入，持有 TurnHandle，转发事件并等待结束。
    ///
    /// - 无输入返回 Ok(None)；忙或关闭时拒绝，不取走排队输入。
    /// - 启动失败返回外层 Err，宿主必须保留尚未启动的输入。
    /// - 运行后无论成功或失败都返回 SessionTurn。
    /// - 返回前将 result 中的 pending **移动**回宿主队列，并清空报告中的
    ///   pending；过滤失效 Reply，不重复保存历史或重复投递用户输入。
    /// - outbox 断开或 cancel 触发时取消当前 Turn 并等待有界清理。
    /// - 此 future 被丢弃时也须请求取消；清理完毕前不得允许重叠运行。
    fn run_next<'a>(
        &'a self,
        limits: TurnLimits,
        outbox: &'a dyn OutSink,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<SessionTurn>, YourAiError>>;

    /// 请求取消当前运行；不清空尚未运行的输入，不等价于完成清理。
    fn interrupt(&self);

    /// 手动压缩，与 Turn 历史写入互斥；ContextManager 内部完成 Hook、提交和内存更新。
    fn compact<'a>(
        &'a self,
        request: CompactionRequest,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>>;

    /// 幂等关闭：拒绝新输入，取消并清理当前执行，触发 SessionEnd，释放资源。
    /// timeout 限制整个关闭过程；None 表示无限制。失败时不得伪报 Closed 或恢复接受任务。
    /// 成功时移交未执行用户输入；调用方决定保存还是丢弃，不能静默丢失。
    fn close<'a>(
        &'a self,
        timeout: Option<Duration>,
    ) -> BoxFuture<'a, Result<Vec<In>, YourAiError>>;
}
