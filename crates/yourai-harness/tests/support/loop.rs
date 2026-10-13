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
use yourai_core::{
    model::{ModelEventStream, ModelRecovery},
    prelude::*,
};
use yourai_harness::default_loop::DefaultLoop;

#[derive(Default)]
pub struct History {
    pub id: SessionId,
    pub messages: Mutex<Vec<ChatMessage>>,
    pub compactions: Mutex<Vec<CompactionTrigger>>,
    pub tokens: AtomicU64,
    pub append_failures: AtomicU64,
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
    fn policy(&self) -> ContextPolicy {
        ContextPolicy {
            keep_recent_tokens: 0,
            summary_min_savings: 1,
            safety_margin: 0,
            ..Default::default()
        }
    }
    fn estimate(&self, request: &ChatRequest, _: &dyn ModelProvider) -> Result<u64, YourAiError> {
        Ok(
            if serde_json::to_value(&request.messages).unwrap()
                == serde_json::to_value(self.messages()).unwrap()
            {
                self.tokens.load(Ordering::SeqCst)
            } else {
                1
            },
        )
    }
    fn save_summary<'a>(
        &'a self,
        expected: &'a [StoredMessage],
        sources: Vec<String>,
        summary: StoredMessage,
        progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            {
                let mut messages = vec![summary.message];
                messages.extend(
                    expected
                        .iter()
                        .filter(|m| !sources.contains(&m.id))
                        .map(StoredMessage::model_message),
                );
                *self.messages.lock().unwrap() = messages;
                self.tokens.store(1, Ordering::SeqCst);
                self.compactions
                    .lock()
                    .unwrap()
                    .push(CompactionTrigger::Threshold);
            }
            progress.store(CommitStatus::Committed as u8, Ordering::Release);
            Ok(())
        })
    }
}

pub struct Model {
    pub steps: Mutex<VecDeque<ModelEventStream>>,
    pub requests: Mutex<Vec<ModelRequest>>,
    pub recovery: ModelRecovery,
    budget: ModelTokenBudget,
}
impl Model {
    pub fn new(steps: Vec<ModelEventStream>) -> Self {
        Self {
            steps: Mutex::new(steps.into()),
            requests: Mutex::new(vec![]),
            recovery: ModelRecovery::Fatal,
            budget: ModelTokenBudget::default(),
        }
    }
    pub fn with_budget(mut self, context: u64, output: u32) -> Self {
        self.budget = ModelTokenBudget::resolve(
            ModelLimits {
                context: Some(context),
                ..Default::default()
            },
            Some(output),
        )
        .unwrap();
        self
    }
}
impl ModelProvider for Model {
    fn token_budget(&self) -> ModelTokenBudget {
        self.budget
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let iden = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test-model");
            Ok(ChatResponse {
                content: MessageContent::from_text("## Goal\ntest summary\n## Constraints and decisions\nNone\n## Completed and verified\nNone\n## Current work\nNone\n## Next steps\nNone\n## Files and commands\nNone"),
                model_iden: iden.clone(),
                provider_model_iden: iden,
                reasoning_content: None,
                stop_reason: None,
                usage: GenaiUsage {
                    prompt_tokens: Some(3),
                    completion_tokens: Some(1),
                    total_tokens: Some(4),
                    ..Default::default()
                },
                captured_raw_body: None,
                response_id: None,
            })
        })
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
impl HookManager for Hooks {
    // This fixed-response fixture does not support dynamic registration.
    fn register<'a>(
        &'a self,
        _: yourai_core::hooks::NativeHookRegistration,
    ) -> yourai_core::BoxFuture<'a, Result<(), yourai_core::YourAiError>> {
        panic!("registration is not used by this test hook")
    }
    fn unregister<'a>(
        &'a self,
        _: &'a str,
    ) -> yourai_core::BoxFuture<'a, Result<bool, yourai_core::YourAiError>> {
        panic!("unregistration is not used by this test hook")
    }
    fn handler_ids<'a>(&'a self) -> yourai_core::BoxFuture<'a, Vec<String>> {
        panic!("registry inspection is not used by this test hook")
    }

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
pub fn builder(model: Arc<Model>, history: Arc<History>) -> AgentBuilder {
    Agent::builder()
        .agent_loop(Arc::new(DefaultLoop))
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
