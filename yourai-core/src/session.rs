//! SessionManager：会话元数据生命周期（创建/加载/保存/列表/删除/fork）。
//!
//! 决策 5.4：对话历史由 ContextManager 自持久化，这里只管元数据。

use crate::error::YourAiError;
use crate::future::BoxFuture;

/// 会话标识
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        SessionId(uuid_v4_like())
    }
}

impl Default for SessionId {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Display for SessionId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// 会话元数据
#[derive(Debug, Clone)]
pub struct SessionMeta {
    pub id: SessionId,
    pub title: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub model: Option<String>,
}

pub trait SessionManager: Send + Sync {
    fn create_session(&self) -> BoxFuture<'_, Result<SessionMeta, YourAiError>>;
    fn load_session(&self, id: &SessionId) -> BoxFuture<'_, Result<SessionMeta, YourAiError>>;
    fn save_session(&self, session: &SessionMeta) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionMeta>, YourAiError>>;
    fn delete_session(&self, id: &SessionId) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn fork_session(&self, id: &SessionId) -> BoxFuture<'_, Result<SessionId, YourAiError>>;
}

// 内部：无外部 UUID 依赖的轻量 id 生成（机制层不引多余依赖）
fn uuid_v4_like() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default().as_nanos();
    let a = nanos as u64;
    let b = std::process::id() as u64;
    format!("{a:016x}{b:08x}")
}
