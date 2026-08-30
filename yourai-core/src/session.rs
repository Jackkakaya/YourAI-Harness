//! SessionManager：会话元数据生命周期（创建/加载/保存/列表/删除/fork）。
//!
//! 决策 5.4：对话历史由 ContextManager 自持久化，这里只管元数据。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use uuid::Uuid;

/// 会话标识（UUID v4，跨主机唯一）
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct SessionId(pub String);

impl SessionId {
    pub fn new() -> Self {
        SessionId(Uuid::new_v4().to_string())
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
    fn create_session<'a>(&'a self) -> BoxFuture<'a, Result<SessionMeta, YourAiError>>;
    fn load_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>>;
    fn save_session<'a>(
        &'a self,
        session: &'a SessionMeta,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SessionMeta>, YourAiError>>;
    fn delete_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>>;
}
