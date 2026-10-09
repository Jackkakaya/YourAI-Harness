#![allow(dead_code)]
use futures_util::{stream, StreamExt};
use serde_json::{json, Value};
use std::{
    collections::{HashMap, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
};
use tokio_util::sync::CancellationToken;
use yourai_core::{
    model::{ModelEventStream, ModelRecovery},
    prelude::*,
};
use yourai_harness::default_loop::{DefaultLoop, LoopConfig};

#[derive(Default)]
pub struct History {
    pub id: SessionId,
    pub messages: Mutex<Vec<ChatMessage>>,
    pub compactions: Mutex<Vec<CompactionTrigger>>,
    pub summary_instructions: Mutex<Vec<Option<String>>>,
    pub compaction_action: Mutex<Option<CompactAction>>,
    pub tokens: AtomicU64,
    pub append_failures: AtomicU64,
    pub usage: Mutex<Vec<Usage>>,
}
impl ContextManager for History {
    fn system_prompt(&self) -> String {
        String::new()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async { Ok(()) })
    }
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async move {
            if self
                .append_failures
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(ErrorKind::Provider {
                    name: "history",
                    message: "temporary write failure".into(),
                }
                .into());
            }
            self.messages
                .lock()
                .unwrap()
                .extend(messages.into_iter().map(|m| m.model_message()));
            Ok(())
        })
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.messages()
            .into_iter()
            .enumerate()
            .map(|(i, m)| {
                let mut r = StoredMessage::new(m);
                r.id = i.to_string();
                r.seq = i as i64 + 1;
                r
            })
            .collect()
    }

    fn session_id(&self) -> &SessionId {
        &self.id
    }
    fn messages(&self) -> Vec<ChatMessage> {
        self.messages.lock().unwrap().clone()
    }
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        _: &dyn ModelProvider,
        suffix: &[ChatMessage],
    ) -> Result<ContextRequest, YourAiError> {
        let mut request = ChatRequest::new(self.messages());
        request.system = Some(self.system_prompt());
        request.tools = Some(tools.to_vec());
        request.messages.extend_from_slice(suffix);
        Ok(ContextRequest {
            request,
            estimated_tokens: self.tokens.load(Ordering::SeqCst),
            input_budget: None,
            maintenance_needed: self.tokens.load(Ordering::SeqCst) >= 80,
        })
    }
    fn prepare_compaction<'a>(
        &'a self,
        req: &'a CompactionRequest,
        _: &'a dyn ModelProvider,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        Box::pin(async move {
            let action = *self.compaction_action.lock().unwrap();
            if let Some(action @ (CompactAction::Unchanged | CompactAction::Pruned)) = action {
                self.compactions.lock().unwrap().push(req.trigger);
                let before = self.tokens.load(Ordering::SeqCst);
                let after = if action == CompactAction::Pruned {
                    self.tokens.store(1, Ordering::SeqCst);
                    1
                } else {
                    before
                };
                return Ok(CompactionPlan::Complete(CompactionResult::new(
                    action,
                    before,
                    after,
                    "no summary",
                )));
            }
            Ok(CompactionPlan::Summary(Box::new(HistorySummary(self))))
        })
    }
}
struct HistorySummary<'a>(&'a History);
impl CompactionJob for HistorySummary<'_> {
    fn run<'a>(
        self: Box<Self>,
        req: CompactionRequest,
        _: &'a dyn ModelProvider,
        _: Option<&'a dyn UsageTracker>,
        _: &'a CancellationToken,
        committed: Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(CompactionResult, String), YourAiError>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            self.0
                .summary_instructions
                .lock()
                .unwrap()
                .push(req.custom_instructions.clone());
            self.0.compactions.lock().unwrap().push(req.trigger);
            let before = self.0.tokens.swap(1, Ordering::SeqCst);
            req.calls.fetch_add(1, Ordering::SeqCst);
            req.usage.lock().unwrap().push(GenaiUsage {
                prompt_tokens: Some(3),
                completion_tokens: Some(1),
                total_tokens: Some(4),
                ..Default::default()
            });
            let mut r =
                CompactionResult::new(CompactAction::Summarized, before.max(2), 1, "test summary");
            r.usage = Some(Usage {
                input_tokens: 3,
                output_tokens: 1,
                total_tokens: 4,
            });
            committed.store(CommitStatus::Committed as u8, Ordering::Release);
            Ok((r, "test summary".into()))
        })
    }
}

pub struct Model {
    pub steps: Mutex<VecDeque<ModelEventStream>>,
    pub requests: Mutex<Vec<ModelRequest>>,
    pub recovery: ModelRecovery,
}
impl Model {
    pub fn new(steps: Vec<ModelEventStream>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(vec![]),
            recovery: ModelRecovery::Fatal,
        }
    }
}
impl ModelProvider for Model {
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async { Err(ErrorKind::Config("unused".into()).into()) })
    }

    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(r);
            self.steps
                .lock()
                .unwrap()
                .pop_front()
                .ok_or_else(|| ErrorKind::Config("script exhausted".into()).into())
        })
    }
    fn recovery(&self, _: &YourAiError) -> ModelRecovery {
        self.recovery
    }
    fn model_iden(&self) -> &str {
        "test-model"
    }
}
pub fn chunk(text: &str) -> ChatStreamEvent {
    ChatStreamEvent::Chunk(StreamChunk {
        content: text.into(),
    })
}
pub fn end(text: &str, calls: Vec<ToolCall>) -> ChatStreamEvent {
    let mut content = MessageContent::from_text(text);
    content.extend(MessageContent::from_tool_calls(calls));
    ChatStreamEvent::End(StreamEnd {
        captured_content: Some(content),
        captured_usage: Some(GenaiUsage {
            prompt_tokens: Some(2),
            completion_tokens: Some(1),
            total_tokens: Some(3),
            ..Default::default()
        }),
        ..Default::default()
    })
}
pub fn events(events: Vec<ChatStreamEvent>) -> ModelEventStream {
    Box::pin(stream::iter(events.into_iter().map(Ok)))
}
pub fn answer(text: &str) -> ModelEventStream {
    events(vec![chunk(text), end(text, vec![])])
}
pub fn calls(names: &[&str]) -> ModelEventStream {
    events(vec![end(
        "",
        names
            .iter()
            .enumerate()
            .map(|(i, n)| call(&format!("c{i}"), n))
            .collect(),
    )])
}
pub fn call(id: &str, name: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: name.into(),
        fn_arguments: json!({"value":1}),
        thought_signatures: None,
    }
}
pub fn failure() -> YourAiError {
    ErrorKind::Provider {
        name: "model",
        message: "test failure".into(),
    }
    .into()
}
pub fn error_stream() -> ModelEventStream {
    Box::pin(stream::iter(vec![Err(failure())]))
}
pub fn hangs_after(text: &str) -> ModelEventStream {
    Box::pin(stream::iter(vec![Ok(chunk(text))]).chain(stream::pending()))
}

#[derive(Clone)]
pub enum Mode {
    Return,
    Fail,
    Ask,
    Mcp,
    Hang,
}
pub struct Handler {
    pub name: String,
    pub mode: Mode,
    pub inputs: Mutex<Vec<Value>>,
}
impl Handler {
    pub fn new(name: &str, mode: Mode) -> Self {
        Self {
            name: name.into(),
            mode,
            inputs: Mutex::new(vec![]),
        }
    }
}
impl ToolProvider for Handler {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new(&self.name).with_schema(json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value, _cwd: Option<&std::path::Path>) -> SecurityContext {
        SecurityContext {
            action: self.name.clone(),
            input: input.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            self.inputs.lock().unwrap().push(input.clone());
            tc.emit_progress(json!("working"));
            match self.mode {
            Mode::Return=>Ok(input),Mode::Fail=>Err(ErrorKind::Tool{name:self.name.clone(),message:"failed".into()}.into()),
            Mode::Ask=>{tc.ask(InteractionKind::Question,json!({"message":"first"})).await?;tc.ask(InteractionKind::Question,json!({"message":"second"})).await},
            Mode::Mcp=>tc.ask(InteractionKind::McpElicitation{server_name:"test".into(),elicitation_id:Some("elicitation".into())},json!({"message":"choose", "requested_schema":{"type":"object","properties":{"choice":{"type":"string"}},"required":["choice"]}})).await,
            Mode::Hang=>std::future::pending().await,
        }
        })
    }
}
#[derive(Default)]
pub struct Registry(pub Mutex<HashMap<String, Tool>>);
impl ToolRegistry for Registry {
    fn register(&self, provider: Arc<dyn ToolProvider>) {
        let tool = Tool::new(provider);
        self.0.lock().unwrap().insert(tool.name().into(), tool);
    }
    fn unregister(&self, name: &str) {
        self.0.lock().unwrap().remove(name);
    }
    fn snapshot(&self) -> Vec<Tool> {
        self.0.lock().unwrap().values().cloned().collect()
    }
}
pub struct Security {
    pub decision: ApprovalDecision,
    pub seen: Mutex<Vec<Value>>,
}
impl Security {
    pub fn new(d: ApprovalDecision) -> Self {
        Self {
            decision: d,
            seen: Mutex::new(vec![]),
        }
    }
}
impl SecurityProvider for Security {
    fn check_tool_call<'a>(
        &'a self,
        c: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(c.input.clone());
            Ok(self.decision)
        })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Allow) })
    }
    fn check_file_access<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Allow) })
    }
}
pub type HookFn = dyn Fn(&HookInvocation, &mut HookDispatchResult) + Send + Sync;
pub struct Hooks {
    pub seen: Mutex<Vec<HookEventKind>>,
    pub f: Box<HookFn>,
}
impl Hooks {
    pub fn new(
        f: impl Fn(&HookInvocation, &mut HookDispatchResult) + Send + Sync + 'static,
    ) -> Self {
        Self {
            seen: Mutex::new(vec![]),
            f: Box::new(f),
        }
    }
}
impl HookRuntime for Hooks {
    fn dispatch<'a>(
        &'a self,
        i: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            self.seen.lock().unwrap().push(i.event.kind());
            let mut result = HookDispatchResult::empty(i.event.kind());
            (self.f)(i, &mut result);
            Ok(result)
        })
    }
}
pub fn builder(model: Arc<Model>, history: Arc<History>, config: LoopConfig) -> AgentBuilder {
    Agent::builder()
        .agent_loop(Arc::new(DefaultLoop::new(config)))
        .model(model)
        .context_manager(history)
}
pub async fn collect(mut handle: TurnHandle) -> (Vec<Out>, TurnResult) {
    let mut events = vec![];
    while let Some(e) = handle.outbox.recv().await {
        events.push(e)
    }
    (events, handle.join().await)
}
pub fn block(result: &mut HookDispatchResult) {
    result.common.blocking_errors.push(HookBlockingError {
        hook_id: "test".into(),
        message: "continue with correction".into(),
    });
}
