//! SessionManager：会话元数据与历史持久化；不负责执行调度。

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
    pub parent_session_id: Option<SessionId>,
    pub provider: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    pub model: Option<String>,
}

pub trait SessionManager: Send + Sync {
    fn read_messages<'a>(
        &'a self,
        id: &'a SessionId,
        query: MessageQuery,
    ) -> BoxFuture<'a, Result<MessagePage, YourAiError>>;
    fn append_messages<'a>(
        &'a self,
        id: &'a SessionId,
        messages: Vec<StoredMessage>,
    ) -> BoxFuture<'a, Result<Vec<StoredMessage>, YourAiError>>;
    fn save_context<'a>(
        &'a self,
        id: &'a SessionId,
        change: ContextChange,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;

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

/// A message identity is allocated before submitting it; retries reuse the identity.
#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: String,
    pub seq: i64,
    pub message: crate::chat::ChatMessage,
    pub summary: bool,
    /// Runtime/Hook context is not a user request, even when sent with role=user.
    pub runtime_context: bool,
    pub status: MessageStatus,
    pub tool_output_pruned_at: Option<i64>,
    /// Transient: the append is a successful model response, not partial cleanup.
    pub model_response: bool,
    pub request_observation: Option<RequestObservation>,
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MessageStatus {
    Active,
    Compacted,
    Archived,
}
impl StoredMessage {
    pub fn runtime_context(text: impl Into<String>) -> Self {
        let mut row = Self::new(crate::chat::ChatMessage::user(text.into()));
        row.runtime_context = true;
        row
    }
    pub fn new(message: crate::chat::ChatMessage) -> Self {
        Self {
            id: Uuid::new_v4().to_string(),
            seq: 0,
            message,
            summary: false,
            runtime_context: false,
            status: MessageStatus::Active,
            tool_output_pruned_at: None,
            model_response: false,
            request_observation: None,
        }
    }
}
/// Ascending sequence cursor. A page never silently truncates without a next cursor.
#[derive(Debug, Clone)]
pub struct MessageQuery {
    pub active_only: bool,
    pub after: i64,
    pub limit: usize,
}
impl Default for MessageQuery {
    fn default() -> Self {
        Self {
            active_only: false,
            after: 0,
            limit: 100,
        }
    }
}
#[derive(Debug, Clone)]
pub struct MessagePage {
    pub messages: Vec<StoredMessage>,
    pub next: Option<i64>,
}
/// Source identities are transient transaction input, never a JSON column.
#[derive(Debug, Clone)]
pub struct CompactionChange {
    pub sources: Vec<String>,
    pub summary: StoredMessage,
}

/// One transaction: pruning alone, or pruning plus a replacement summary.
#[derive(Debug, Clone, Default)]
pub struct ContextChange {
    pub compaction: Option<CompactionChange>,
    pub pruned: Vec<String>,
}
impl From<CompactionChange> for ContextChange {
    fn from(compaction: CompactionChange) -> Self {
        Self {
            compaction: Some(compaction),
            pruned: vec![],
        }
    }
}

/// Transient observation accompanying a successful assistant append; not a second usage ledger.
#[derive(Debug, Clone)]
pub struct RequestObservation {
    pub model: String,
    pub request: crate::chat::ChatRequest,
    pub input_tokens: u64,
}
