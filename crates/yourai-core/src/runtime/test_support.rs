use super::*;
struct Model;
impl ModelProvider for Model {
    fn model_iden(&self) -> &str {
        "test"
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async { unreachable!() })
    }
    fn stream_events<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async { std::future::pending().await })
    }
}
struct History {
    id: SessionId,
    rows: Mutex<Vec<StoredMessage>>,
}
impl ContextManager for History {
    fn session_id(&self) -> &SessionId {
        &self.id
    }
    fn system_prompt(&self) -> String {
        "test".into()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }
    fn append(&self, rows: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async move {
            self.rows.lock().unwrap().extend(rows);
            Ok(())
        })
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.rows.lock().unwrap().clone()
    }
    fn build_request(
        &self,
        _: &[ToolDefinition],
        _: &dyn ModelProvider,
        _suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError> {
        unreachable!()
    }
    fn prepare_compaction<'a>(
        &'a self,
        _: &'a CompactionRequest,
        _: &'a dyn ModelProvider,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        unreachable!()
    }
}
struct IdleLoop;
impl AgentLoop for IdleLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let input = tc.inbox.recv().await.unwrap();
            tc.cancel.cancelled().await;
            let mut out = TurnOutput::new("");
            out.pending.push(input);
            Err(TurnFailure::new(AbortReason::Cancelled, out))
        })
    }
}
pub(crate) struct Fixture {
    pub host: Arc<SessionHost>,
}
impl Fixture {
    pub async fn close(&self) -> Result<Vec<In>, YourAiError> {
        self.host.close(None).await
    }
}
pub(crate) async fn fixture() -> (tempfile::TempDir, Fixture) {
    fixture_with_hooks(None).await
}
pub(crate) async fn fixture_with_hooks(
    hooks: Option<Arc<dyn HookRuntime>>,
) -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let history = Arc::new(History {
        id: SessionId::new(),
        rows: Mutex::new(vec![]),
    });
    let context = SessionContext::new(history.id.clone(), dir.path().to_path_buf());
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(IdleLoop))
        .model(Arc::new(Model))
        .context_manager(history);
    if let Some(hooks) = hooks {
        builder = builder.hooks(hooks);
    }
    let agent = builder.build();
    let host = SessionHost::open(
        dir.path().join("session"),
        context,
        agent,
        HostConfig {
            workspace_enabled: true,
            ..Default::default()
        },
        "startup",
    )
    .await
    .unwrap();
    (dir, Fixture { host })
}

#[derive(Default)]
pub(crate) struct Hooks {
    pub results: Mutex<std::collections::HashMap<HookEventKind, HookDispatchResult>>,
    pub fail: Mutex<Option<HookEventKind>>,
    pub seen: Mutex<Vec<HookEventKind>>,
    pub shutdown: std::sync::atomic::AtomicUsize,
}
impl HookRuntime for Hooks {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            let kind = invocation.event.kind();
            self.seen.lock().unwrap().push(kind);
            if *self.fail.lock().unwrap() == Some(kind) {
                return Err(error("hook", "boom"));
            }
            Ok(self
                .results
                .lock()
                .unwrap()
                .get(&kind)
                .cloned()
                .unwrap_or_else(|| HookDispatchResult::empty(kind)))
        })
    }
    fn shutdown_session<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.shutdown
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        })
    }
}
