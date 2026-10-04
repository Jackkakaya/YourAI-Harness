//! Context mutations are committed before the active in-memory view changes.
use crate::prelude::{BaseInput, HookRuntime, ModelProvider, ProviderSnapshot, SessionContext};
use crate::{chat::*, compaction::*, error::YourAiError, future::BoxFuture, session::*};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Validated view of shared execution bindings; context storage retains no providers.
/// Required capabilities cannot be removed after construction:
/// ```compile_fail
/// use yourai_core::prelude::*;
/// fn invalidate(context: &mut ContextExecution) {
///     context.bindings().model = None;
/// }
/// ```
#[derive(Clone)]
pub struct ContextExecution {
    bindings: crate::context::ExecutionBindings,
    pub hooks: Option<Arc<dyn HookRuntime>>,
    pub hook_base: BaseInput,
}
impl ContextExecution {
    /// Validate the required capability once; consumers cannot remove it later.
    pub fn new(
        bindings: crate::context::ExecutionBindings,
        hooks: Option<Arc<dyn HookRuntime>>,
        hook_base: BaseInput,
    ) -> Result<Self, YourAiError> {
        if bindings.model.is_none() {
            return Err(crate::ErrorKind::Config("model not configured".into()).into());
        }
        Ok(Self {
            bindings,
            hooks,
            hook_base,
        })
    }
    pub fn model(&self) -> &Arc<dyn ModelProvider> {
        self.bindings
            .model
            .as_ref()
            .expect("validated context model")
    }
    pub fn bindings(&self) -> &crate::context::ExecutionBindings {
        &self.bindings
    }

    pub fn from_snapshot(
        snapshot: &ProviderSnapshot,
        id: &SessionId,
        session: Option<&SessionContext>,
    ) -> Result<Self, YourAiError> {
        let mut hook_base = BaseInput::new(id.as_str(), "");
        if let Some(session) = session {
            hook_base.cwd = session.cwd.to_string_lossy().into_owned();
            hook_base.transcript_path = session
                .transcript_path
                .as_ref()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default();
        }
        Self::new(
            crate::context::ExecutionBindings::from_snapshot(snapshot),
            snapshot.hooks.clone(),
            hook_base,
        )
    }
}

/// All transient request content is supplied before projection and admission.
#[derive(Debug, Clone, Copy, Default)]
pub struct RequestInput<'a> {
    pub tools: &'a [Tool],
    pub suffix: &'a [ChatMessage],
}
impl<'a> RequestInput<'a> {
    pub fn tools(tools: &'a [Tool]) -> Self {
        Self { tools, suffix: &[] }
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
        input: RequestInput<'_>,
        execution: &ContextExecution,
    ) -> Result<ContextRequest, YourAiError>;
    /// Implementation-side planning. Public context operations own the hook lifecycle.
    /// Complete plans include no-op/prune-only commits; Summary jobs hold any
    /// transaction/lock needed until their business commit finishes.
    fn prepare_compaction<'a>(
        &'a self,
        options: &'a CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>>;
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

/// A prepared business operation; no hook protocol is required from implementations.
pub enum CompactionPlan<'a> {
    Complete(CompactionResult),
    Summary(Box<dyn CompactionJob + 'a>),
}

pub struct CompactionCommit {
    pub result: CompactionResult,
    pub summary: String,
}

/// Implementation-side summary and durable commit.
pub trait CompactionJob: Send {
    fn run<'a>(
        self: Box<Self>,
        options: CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
        committed: &'a std::sync::atomic::AtomicBool,
    ) -> BoxFuture<'a, Result<CompactionCommit, YourAiError>>
    where
        Self: 'a;
}
