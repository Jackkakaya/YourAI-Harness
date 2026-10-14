#![allow(dead_code)]
#[path = "model.rs"]
mod model_fixture;
pub use model_fixture::TestModel;
use std::sync::Arc;
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
/// Test fixture owns an execution separately from the context under test.
pub struct ContextServices {
    pub store: Option<Arc<dyn SessionStore>>,
    pub policy: ContextPolicy,
    pub hooks: Option<Arc<dyn HookManager>>,
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
        let inner =
            yourai_harness::DefaultContext::new(id.clone(), String::new(), s.store, s.policy);
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
    fn save_summary<'a>(
        &'a self,
        expected: &'a [StoredMessage],
        sources: Vec<String>,
        summary: StoredMessage,
        progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.inner
            .save_summary(expected, sources, summary, progress)
    }
    fn project(
        &self,
        records: &[StoredMessage],
        tools: &[ToolDefinition],
    ) -> Result<ChatRequest, YourAiError> {
        self.inner.project(records, tools)
    }
    fn estimate(
        &self,
        request: &ChatRequest,
        model: &dyn ModelProvider,
    ) -> Result<u64, YourAiError> {
        self.inner.estimate(request, model)
    }
}

/// Set model capacity independently from context maintenance policy.
pub fn configured(
    model: Arc<dyn ModelProvider>,
    context: Option<u64>,
    output: u32,
) -> Arc<dyn ModelProvider> {
    TestModel::new(
        model,
        ModelTokenBudget::resolve(
            {
                let mut settings: ModelLimits = Default::default();
                settings.set_context(context);
                settings.set_output(Some(output));
                settings
            },
            None,
        )
        .unwrap(),
        ModelTimeouts::default(),
    )
    .unwrap()
}
