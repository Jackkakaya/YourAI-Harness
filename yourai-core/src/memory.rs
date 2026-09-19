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

pub trait MemoryManager: Send + Sync {
    /// 存一条记忆；`category` 用于 [`list`](MemoryManager::list) 的分类筛选
    fn store<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn retrieve<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<String>, YourAiError>>;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>>;
    fn list<'a>(
        &'a self,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), YourAiError>>;
}

/// Optional memory capabilities and lifecycle callbacks. The framework adapts
/// callbacks to HookRuntime; provider authors never register hooks themselves.
/// Construct ready-to-use connections before installation: system_prompt_block
/// can run before on_session_start. Callbacks must be cancellation-safe; sync_turn
/// implementations deduplicate writes using (session_id, turn_id).
pub trait MemoryProvider: Send + Sync {
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
