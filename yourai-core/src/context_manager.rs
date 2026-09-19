//! Context mutations are committed before the active in-memory view changes.
use crate::prelude::{
    BaseInput, HookRuntime, ModelProvider, ProviderSnapshot, SessionContext, UsageTracker,
};
use crate::{chat::*, compaction::*, error::YourAiError, future::BoxFuture, session::*};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Dependencies selected once for this execution; context storage does not retain providers.
#[derive(Clone)]
pub struct ContextExecution {
    pub model: Arc<dyn ModelProvider>,
    pub hooks: Option<Arc<dyn HookRuntime>>,
    pub usage: Option<Arc<dyn UsageTracker>>,
    pub hook_base: BaseInput,
}
impl ContextExecution {
    pub fn from_snapshot(
        snapshot: &ProviderSnapshot,
        id: &SessionId,
        session: Option<&SessionContext>,
    ) -> Result<Self, YourAiError> {
        let mut hook_base = BaseInput::new(&id.0, "");
        if let Some(session) = session {
            hook_base.cwd = session.cwd.to_string_lossy().into_owned();
            hook_base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        Ok(Self {
            model: snapshot
                .model
                .clone()
                .ok_or_else(|| crate::ErrorKind::Config("model not configured".into()))?,
            hooks: snapshot.hooks.clone(),
            usage: snapshot.usage.clone(),
            hook_base,
        })
    }
}

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
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn build_request(
        &self,
        tools: &[Tool],
        execution: &ContextExecution,
    ) -> Result<ContextRequest, YourAiError>;
    fn compact<'a>(
        &'a self,
        options: CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>>;
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
    fn default_options(&self) -> ChatOptions {
        ChatOptions::default()
    }
}
