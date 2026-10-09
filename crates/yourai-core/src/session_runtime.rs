//! 会话环境与报告类型；具体控制流程由 core SessionHost 固定实现。
//!
//! SessionManager 管元数据；SessionHost 管理活着的会话。创建/恢复由公开的
//! 构造入口完成：绑定历史、装配 Agent、触发 SessionStart 后才对外提供对象。
//!
//! ```
//! use yourai_core::prelude::*;
//! use yourai_core::runtime::SessionHost;
//!
//! fn enqueue(runtime: &SessionHost, text: &str) -> Result<(), InputRejected> {
//!     runtime.submit(In::user_text(text))
//! }
//! ```

use crate::{agent_loop::TurnResult, session::SessionId, turn::TurnId};
use std::path::PathBuf;

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

pub use crate::protocol::InputRejected;

/// 一个已结束 Turn 的报告；执行故障也在 result 中，而不是 run_next 的外层 Err。
#[derive(Debug)]
pub struct SessionTurn {
    pub turn_id: TurnId,
    pub result: TurnResult,
}
