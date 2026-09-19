#![allow(dead_code)]
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
/// Test fixture owns an execution separately from the context under test.
pub struct ContextServices {
    pub store: Option<Arc<dyn SessionManager>>,
    pub policy: ContextPolicy,
    pub hooks: Option<Arc<dyn HookRuntime>>,
    pub usage: Option<Arc<dyn UsageTracker>>,
}
impl ContextServices {
    pub fn new(_: &SessionId) -> Self {
        Self {
            store: None,
            policy: ContextPolicy::default(),
            hooks: None,
            usage: None,
        }
    }
}
pub struct MemoryContext {
    pub inner: Arc<yourai_harness::MemoryContext>,
    pub execution: ContextExecution,
}
impl std::ops::Deref for MemoryContext {
    type Target = yourai_harness::MemoryContext;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl MemoryContext {
    pub fn new(id: SessionId, model: Arc<dyn ModelProvider>, s: ContextServices) -> Arc<Self> {
        let services = yourai_harness::context::ContextServices {
            store: s.store,
            policy: s.policy,
            ..Default::default()
        };
        Arc::new(Self {
            execution: ContextExecution {
                model,
                hooks: s.hooks,
                usage: s.usage,
                hook_base: BaseInput::new(&id.0, ""),
            },
            inner: yourai_harness::MemoryContext::new(id, services),
        })
    }
    pub fn build_request(&self, tools: &[Tool]) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(tools, &self.execution)
    }
    pub fn compact<'a>(
        &'a self,
        r: CompactionRequest,
        c: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        self.inner.compact(r, &self.execution, c)
    }
}
impl ContextManager for MemoryContext {
    fn last_sequence(&self) -> i64 {
        self.inner.last_sequence()
    }
    fn system_prompt(&self) -> String {
        self.inner.system_prompt()
    }
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.inner.restore()
    }
    fn append(&self, m: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.inner.append(m)
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.inner.records()
    }
    fn contains_tool_call(&self, id: &str) -> bool {
        self.inner.contains_tool_call(id)
    }
    fn contains_context_marker(&self, id: &str) -> bool {
        self.inner.contains_context_marker(id)
    }
    fn default_options(&self) -> ChatOptions {
        self.inner.default_options()
    }
    fn policy(&self) -> ContextPolicy {
        self.inner.policy()
    }
    fn build_request(
        &self,
        t: &[Tool],
        e: &ContextExecution,
    ) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(t, e)
    }
    fn compact<'a>(
        &'a self,
        r: CompactionRequest,
        e: &'a ContextExecution,
        c: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        self.inner.compact(r, e, c)
    }
}
