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
            execution: ContextExecution::new(
                ExecutionBindings {
                    model: Some(model),
                    usage: s.usage,
                    ..Default::default()
                },
                s.hooks,
                BaseInput::new(id.as_str(), ""),
            )
            .unwrap(),
            inner: yourai_harness::MemoryContext::new(id, services),
        })
    }
    pub fn build_request(&self, tools: &[Tool]) -> Result<ContextRequest, YourAiError> {
        self.inner
            .build_request(RequestInput::tools(tools), &self.execution)
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
    fn policy(&self) -> ContextPolicy {
        self.inner.policy()
    }
    fn build_request(
        &self,
        t: RequestInput<'_>,
        e: &ContextExecution,
    ) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(t, e)
    }
    fn prepare_compaction<'a>(
        &'a self,
        r: &'a CompactionRequest,
        e: &'a ContextExecution,
        c: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        self.inner.prepare_compaction(r, e, c)
    }
}

/// Bind capacity to the model, as production configuration does.
pub fn configured(
    model: Arc<dyn ModelProvider>,
    context: Option<u64>,
    output: u32,
) -> Arc<dyn ModelProvider> {
    yourai_harness::model::ConfiguredModel::new(
        model,
        ModelTokenBudget::resolve(
            ModelLimits {
                context,
                input: None,
                output: Some(output),
            },
            None,
        )
        .unwrap(),
        ModelTimeouts::default(),
    )
    .unwrap()
}
