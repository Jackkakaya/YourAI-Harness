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
pub struct DefaultContext {
    pub inner: Arc<yourai_harness::DefaultContext>,
    pub execution: Compactor,
    model: Arc<dyn ModelProvider>,
}
impl std::ops::Deref for DefaultContext {
    type Target = yourai_harness::DefaultContext;
    fn deref(&self) -> &Self::Target {
        &self.inner
    }
}
impl DefaultContext {
    pub fn new(id: SessionId, model: Arc<dyn ModelProvider>, s: ContextServices) -> Arc<Self> {
        let services = yourai_harness::context::ContextServices {
            store: s.store,
            policy: s.policy,
            ..Default::default()
        };
        let inner = yourai_harness::DefaultContext::new(id.clone(), services);
        Arc::new(Self {
            execution: Compactor::new(
                inner.clone(),
                model.clone(),
                s.hooks,
                s.usage,
                BaseInput::new(id.as_str(), ""),
            ),
            inner,
            model,
        })
    }
    pub fn build_request(&self, tools: &[ToolDefinition]) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(tools, self.model.as_ref(), &[])
    }
    pub fn compact<'a>(
        &'a self,
        r: CompactionRequest,
        c: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionResult, YourAiError>> {
        Box::pin(self.execution.exec(r, c, None))
    }
}
impl ContextManager for DefaultContext {
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
        t: &[ToolDefinition],
        e: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(t, e, suffix)
    }
    fn prepare_compaction<'a>(
        &'a self,
        r: &'a CompactionRequest,
        e: &'a dyn ModelProvider,
        c: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        self.inner.prepare_compaction(r, e, c)
    }
}

/// Set model capacity independently from context maintenance policy.
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
                output: Some(output),
                ..Default::default()
            },
            None,
        )
        .unwrap(),
        ModelTimeouts::default(),
    )
    .unwrap()
}
