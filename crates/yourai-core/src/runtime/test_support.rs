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
    fn save_summary<'a>(
        &'a self,
        _: &'a [StoredMessage],
        _sources: Vec<String>,
        _summary: StoredMessage,
        _progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
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
    pub host: Arc<SessionManager>,
    pub store: Arc<Store>,
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
    hooks: Option<Arc<dyn HookManager>>,
) -> (tempfile::TempDir, Fixture) {
    let dir = tempfile::tempdir().unwrap();
    let history = Arc::new(History {
        id: SessionId::new(),
        rows: Mutex::new(vec![]),
    });
    let context = SessionEnvironment::new(history.id.clone(), dir.path().to_path_buf());
    let store = Arc::new(Store::default());
    let mut builder = Agent::builder()
        .agent_loop(Arc::new(IdleLoop))
        .model(Arc::new(Model))
        .context_manager(history)
        .session(store.clone());
    if let Some(hooks) = hooks {
        builder = builder.hooks(hooks);
    }
    let agent = builder.build();
    let host = SessionManager::open(
        dir.path().join("session"),
        context,
        agent,
        ExecutionOptions {
            workspace_enabled: true,
            ..Default::default()
        },
        "startup",
    )
    .await
    .unwrap();
    (dir, Fixture { host, store })
}

#[derive(Default)]
pub(crate) struct Hooks {
    pub results: Mutex<std::collections::HashMap<HookEventKind, HookDispatchResult>>,
    pub fail: Mutex<Option<HookEventKind>>,
    pub seen: Mutex<Vec<HookEventKind>>,
    pub shutdown: std::sync::atomic::AtomicUsize,
}
impl HookManager for Hooks {
    // This fixed-response fixture does not support dynamic registration.
    fn register<'a>(
        &'a self,
        _: crate::hooks::NativeHookRegistration,
    ) -> crate::BoxFuture<'a, Result<(), crate::YourAiError>> {
        panic!("registration is not used by this test hook")
    }
    fn unregister<'a>(
        &'a self,
        _: &'a str,
    ) -> crate::BoxFuture<'a, Result<bool, crate::YourAiError>> {
        panic!("unregistration is not used by this test hook")
    }
    fn handler_ids<'a>(&'a self) -> crate::BoxFuture<'a, Vec<String>> {
        panic!("registry inspection is not used by this test hook")
    }

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

#[derive(Clone, Default)]
pub(crate) struct Store {
    states: Arc<Mutex<std::collections::HashMap<String, SessionState>>>,
    owner: Arc<Mutex<Option<String>>>,
    scope: Option<String>,
    pub fail: Arc<std::sync::atomic::AtomicBool>,
}
impl Store {
    fn valid(&self) -> Result<(), YourAiError> {
        if self
            .scope
            .as_ref()
            .is_some_and(|s| self.owner.lock().unwrap().as_ref() != Some(s))
        {
            return Err(error("storage", "stale owner"));
        }
        Ok(())
    }
}
impl SessionStore for Store {
    fn claim_session(
        &self,
        _: &SessionId,
        owner: &str,
        _: u64,
    ) -> Result<Arc<dyn SessionStore>, YourAiError> {
        let mut current = self.owner.lock().unwrap();
        if current.is_some() {
            return Err(error("storage", "already owned"));
        }
        *current = Some(owner.into());
        let mut next = self.clone();
        next.scope = Some(owner.into());
        Ok(Arc::new(next))
    }
    fn renew_session(&self, _: u64) -> Result<(), YourAiError> {
        self.valid()
    }
    fn release_session(&self) -> Result<(), YourAiError> {
        let mut owner = self.owner.lock().unwrap();
        if owner.as_ref() == self.scope.as_ref() {
            *owner = None;
        }
        Ok(())
    }
    fn read_state(&self, _: &SessionId, key: &str) -> Result<Option<SessionState>, YourAiError> {
        self.valid()?;
        Ok(self.states.lock().unwrap().get(key).cloned())
    }
    fn save_state(
        &self,
        _: &SessionId,
        key: &str,
        expected: Option<i64>,
        value: serde_json::Value,
    ) -> Result<i64, YourAiError> {
        self.valid()?;
        if self.fail.load(std::sync::atomic::Ordering::Relaxed) {
            return Err(error("storage", "injected write failure"));
        }
        let mut states = self.states.lock().unwrap();
        if states.get(key).map(|s| s.revision) != expected {
            return Err(error("storage", "stale state"));
        }
        let revision = expected.unwrap_or(0) + 1;
        states.insert(key.into(), SessionState { revision, value });
        Ok(revision)
    }
    fn append_output<'a>(
        &'a self,
        _: &'a SessionId,
        _: &'a str,
        _: u64,
        _: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        panic!("unused outputs in journal fixture")
    }
    fn read_output<'a>(
        &'a self,
        _: &'a SessionId,
        _: &'a str,
        _: u64,
        _: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, YourAiError>> {
        panic!("unused outputs in journal fixture")
    }
    fn cleanup_outputs<'a>(
        &'a self,
        _: &'a SessionId,
        _: i64,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        panic!("unused outputs in journal fixture")
    }
    fn read_tasks(&self, _id: &SessionId) -> Result<Vec<crate::tasks::Task>, YourAiError> {
        panic!("unused persistence in runtime fixture")
    }
    fn save_tasks(
        &self,
        _id: &SessionId,
        _tasks: Vec<crate::tasks::Task>,
    ) -> Result<Vec<crate::tasks::Task>, YourAiError> {
        panic!("unused persistence in runtime fixture")
    }
    fn initialize_system<'a>(
        &'a self,
        _id: &'a SessionId,
        _system: &'a str,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn read_messages<'a>(
        &'a self,
        _id: &'a SessionId,
        _query: MessageQuery,
    ) -> BoxFuture<'a, Result<MessagePage, YourAiError>> {
        Box::pin(async {
            Ok(MessagePage {
                messages: vec![],
                next: None,
            })
        })
    }
    fn append_messages<'a>(
        &'a self,
        _id: &'a SessionId,
        _messages: Vec<StoredMessage>,
    ) -> BoxFuture<'a, Result<Vec<StoredMessage>, YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn save_summary<'a>(
        &'a self,
        _id: &'a SessionId,
        _sources: Vec<String>,
        _summary: StoredMessage,
        _progress: Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn create_session<'a>(
        &'a self,
        _id: SessionId,
        _system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn load_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        Box::pin(async move {
            Ok(SessionMeta {
                id: id.clone(),
                system_prompt: Some(String::new()),
                title: None,
                parent_session_id: None,
                provider: None,
                created_at: 0,
                updated_at: 0,
                model: None,
            })
        })
    }
    fn save_session<'a>(
        &'a self,
        _session: &'a SessionMeta,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SessionMeta>, YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn delete_session<'a>(&'a self, _id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
    fn fork_session<'a>(
        &'a self,
        _id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        panic!("unused persistence in runtime fixture")
    }
}
