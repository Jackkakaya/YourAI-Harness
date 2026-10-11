//! MemoryManager：跨会话的长期记忆（用户偏好、项目上下文、事实等）。

use crate::error::YourAiError;
use crate::future::BoxFuture;

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub category: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

/// Runtime memory recall and lifecycle interface. Local CRUD belongs to the concrete store,
/// rather than a second core trait. The framework adapts
/// callbacks to HookManager; provider authors never register hooks themselves.
/// Construct ready-to-use connections before installation: system_prompt_block
/// can run before on_session_start. Callbacks must be cancellation-safe; sync_turn
/// implementations deduplicate writes using (session_id, turn_id).
pub trait MemoryManager: Send + Sync {
    fn on_session_start<'a>(
        &'a self,
        _session: &'a MemorySession,
        _source: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }

    fn on_session_end<'a>(
        &'a self,
        _session: &'a MemorySession,
        _reason: &'a str,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }

    fn on_pre_compact<'a>(
        &'a self,
        _session: &'a MemorySession,
        _trigger: &'a str,
        _messages: &'a [crate::chat::ChatMessage],
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }

    /// Delivered only after a successful, committed turn. Background/runtime
    /// context and recalled API sidecars are excluded. No delivery guarantee
    /// across a process crash; a callback failure cannot undo the conversation.
    fn sync_turn<'a>(
        &'a self,
        _turn: &'a CompletedMemoryTurn,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }

    fn system_prompt_block<'a>(
        &'a self,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<Option<String>, YourAiError>> {
        Box::pin(async { Ok(None) })
    }
    fn recall<'a>(
        &'a self,
        _request: RecallRequest<'a>,
        _cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<RecalledMemory>, YourAiError>> {
        Box::pin(async { Ok(vec![]) })
    }
}
#[derive(Debug, Clone, Copy)]
pub struct RecallRequest<'a> {
    pub query: &'a str,
    pub limit: usize,
    pub max_chars: usize,
}
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct RecalledMemory {
    pub provider: String,
    pub id: String,
    pub content: String,
    pub updated_at: Option<i64>,
}

/// Host-owned scope. Providers must not broaden access based on recalled text.
#[derive(Debug, Clone)]
pub struct MemorySession {
    pub session_id: crate::session::SessionId,
    pub cwd: std::path::PathBuf,
}
#[derive(Debug, Clone)]
pub struct CompletedMemoryTurn {
    pub session: MemorySession,
    pub turn_id: String,
    /// Clean committed messages, including tool pairs and accepted steer.
    pub messages: Vec<crate::chat::ChatMessage>,
}
