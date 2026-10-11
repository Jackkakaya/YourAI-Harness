//! Active context projection and atomic durable mutations.
use crate::{
    chat::*, compactor::ContextPolicy, error::YourAiError, future::BoxFuture, model::ModelProvider,
    session::*,
};
use std::sync::{atomic::AtomicU8, Arc};

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub request: ChatRequest,
    pub estimated_tokens: u64,
    pub input_budget: Option<u64>,
    pub maintenance_needed: bool,
}
impl ContextRequest {
    pub fn fits(&self) -> bool {
        self.input_budget.is_none_or(|b| self.estimated_tokens <= b)
    }
}
pub trait ContextManager: Send + Sync {
    fn system_prompt(&self) -> String;
    fn session_id(&self) -> &SessionId;
    /// Synchronize previously started owned writes before publishing a readable view.
    /// Close uses this as the storage settlement barrier before releasing the session lease.
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>>;
    /// Appends stored messages atomically to the durable history.
    /// Retries reuse message identities; identical batches must not duplicate rows.
    /// Identity reuse with a different message or admission input must fail.
    ///
    /// Implementations must accept runtime-context rows (marker-prefixed
    /// notes such as `[PostCompact context]`): the public compact wrapper
    /// appends PostCompact additional contexts through this method, and a
    /// rejection would stop continuation after an already committed summary.
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        model: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError>;
    /// Project a candidate view using the same rules as normal model requests.
    fn project(
        &self,
        records: &[StoredMessage],
        tools: &[ToolDefinition],
    ) -> Result<ChatRequest, YourAiError> {
        let current = crate::session::current_context_ids(records);
        let mut request = ChatRequest::new(
            records
                .iter()
                .filter(|m| m.context_key.is_none() || current.contains(m.id.as_str()))
                .map(StoredMessage::model_message)
                .collect::<Vec<_>>(),
        );
        request.system = Some(self.system_prompt());
        request.tools = Some(tools.to_vec());
        Ok(request)
    }
    fn estimate(
        &self,
        request: &ChatRequest,
        model: &dyn ModelProvider,
    ) -> Result<u64, YourAiError> {
        crate::model::estimate_request(request, model)
    }
    /// Compare the active snapshot and commit under one mutation lock. A stale
    /// snapshot must fail before starting a write; cancellation never undoes SQL.
    fn save_summary<'a>(
        &'a self,
        expected: &'a [StoredMessage],
        sources: Vec<String>,
        summary: StoredMessage,
        progress: Arc<AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    /// Read-only active view and archival identity checks used by recovery/deduplication.
    fn records(&self) -> Vec<StoredMessage>;
    /// Committed high-water mark, including messages removed from active context.
    fn last_sequence(&self) -> i64 {
        self.records().iter().map(|m| m.seq).max().unwrap_or(0)
    }
    fn messages(&self) -> Vec<ChatMessage> {
        self.records().into_iter().map(|r| r.message).collect()
    }
    fn contains_tool_call(&self, id: &str) -> bool {
        self.records().iter().any(|m| {
            m.message
                .content
                .tool_calls()
                .iter()
                .any(|c| c.call_id == id)
        })
    }
    fn contains_context_marker(&self, marker: &str) -> bool {
        self.records().iter().any(|m| {
            m.message
                .content
                .first_text()
                .is_some_and(|s| s.starts_with(marker))
        })
    }
    fn policy(&self) -> ContextPolicy {
        ContextPolicy::default()
    }
}
