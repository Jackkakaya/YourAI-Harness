//! Public business execution, independent of any scheduling strategy.
//! AgentLoop implementations call these operations without dispatching hooks.
mod admission;
mod attachment;
mod config;
mod control;
mod hooks;
mod interaction;
mod model;
mod tools;

pub use config::{AttachmentImageConfig, ExecutionConfig};
use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
};
use yourai_core::{model::ModelRecovery, prelude::*};

/// Per-turn business capabilities and call ledger. Any AgentLoop can create it.
/// It owns input routing, durable results and operation lifecycles, not scheduling.
pub struct TurnExecution<'a> {
    state: ExecutionState<'a>,
    accepted: bool,
    completed: bool,
    span: Option<Arc<dyn yourai_core::observability::Span>>,
}

pub use yourai_core::completion::Completion;

/// Model step options/output live in core next to the public execution entry.
pub use yourai_core::model::{ModelOptions, ModelOutput};

pub use yourai_core::tool::ExecutedTool as ToolOutput;

pub struct ToolExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
pub struct ModelExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
pub struct ContextExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
pub struct InputExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
pub struct PermissionExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
pub struct InteractionExecutor<'a, 'turn> {
    state: &'a mut ExecutionState<'turn>,
}
impl yourai_core::inputs::InputOperation for InputExecutor<'_, '_> {
    fn accept_bound<'a>(&'a mut self, input: In) -> BoxFuture<'a, Result<bool, YourAiError>> {
        Box::pin(async move {
            if !matches!(input, In::UserText { .. }) {
                return Err(ErrorKind::Config("input admission requires UserText".into()).into());
            }
            let index = self.state.queued.len();
            self.state.queued.push_back(input);
            self.state.accept_input(index, false).await
        })
    }
}
impl InputExecutor<'_, '_> {
    pub async fn accept(&mut self, input: In) -> Result<bool, YourAiError> {
        yourai_core::inputs::accept(self, input).await
    }
}
/// Stop 生命周期的框架操作持有整个 TurnExecution（完成状态与可观测性）。
pub(crate) struct CompletionExecutor<'a, 'turn> {
    turn: &'a mut TurnExecution<'turn>,
}
impl yourai_core::completion::CompletionOperation for CompletionExecutor<'_, '_> {
    fn complete_bound<'a>(
        &'a mut self,
        text: String,
    ) -> BoxFuture<'a, Result<Completion, YourAiError>> {
        Box::pin(async move {
            self.turn.state.tc.check_control()?;
            if !self.turn.state.unresolved.is_empty() {
                return Err(
                    ErrorKind::Loop("cannot complete with unresolved tool calls".into()).into(),
                );
            }
            self.turn.state.complete_candidate(text).await?;
            let completion = if self.turn.state.finish().await? {
                Completion::Completed
            } else {
                Completion::NeedsMoreWork
            };
            self.turn.completed = completion == Completion::Completed;
            Ok(completion)
        })
    }
}
impl yourai_core::security::SecurityOperation for PermissionExecutor<'_, '_> {
    fn authorize_bound<'a>(
        &'a mut self,
        call: &'a mut ToolCall,
        binding: &'a yourai_core::tool::ToolBinding,
        hook_permission: HookPermission,
        doom_loop: bool,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(
            self.state
                .approve(call, binding, hook_permission, doom_loop),
        )
    }
}
impl PermissionExecutor<'_, '_> {
    pub async fn authorize(
        &mut self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<serde_json::Value, YourAiError> {
        let binding = self
            .state
            .tc
            .snap
            .tools
            .as_ref()
            .ok_or_else(|| ErrorKind::Config("tools not configured".into()))?
            .resolve(name)?;
        let mut call = ToolCall {
            call_id: uuid::Uuid::new_v4().to_string(),
            fn_name: name.into(),
            fn_arguments: input,
            thought_signatures: None,
        };
        if let Some(schema) = &binding.definition().schema {
            interaction::validate_schema(schema, &call.fn_arguments)?;
        }
        // Fixed public entry: the PermissionRequest/Denied lifecycle lives in
        // the framework operation, never in the caller.
        binding
            .authorize(self, &mut call, HookPermission::default(), false)
            .await?;
        Ok(call.fn_arguments)
    }
}
impl yourai_core::interaction::InteractionOperation for InteractionExecutor<'_, '_> {
    fn elicit_bound<'a>(
        &'a mut self,
        request: InteractionRequest,
    ) -> BoxFuture<'a, Result<serde_json::Value, YourAiError>> {
        Box::pin(self.state.service_interaction(request))
    }
}
impl InteractionExecutor<'_, '_> {
    pub async fn elicit(
        &mut self,
        request: InteractionRequest,
    ) -> Result<serde_json::Value, YourAiError> {
        // Fixed public entry: Elicitation/ElicitationResult lifecycle lives in
        // the framework operation.
        yourai_core::interaction::elicit(self, request).await
    }
}

impl<'turn> TurnExecution<'turn> {
    pub fn tools(&mut self) -> ToolExecutor<'_, 'turn> {
        ToolExecutor {
            state: &mut self.state,
        }
    }
    pub fn model(&mut self) -> ModelExecutor<'_, 'turn> {
        ModelExecutor {
            state: &mut self.state,
        }
    }
    pub fn context(&mut self) -> ContextExecutor<'_, 'turn> {
        ContextExecutor {
            state: &mut self.state,
        }
    }
    pub fn input_accepted(&self) -> bool {
        self.accepted
    }
    pub fn text(&self) -> &str {
        &self.state.output.text
    }
    pub fn turn_id(&self) -> &TurnId {
        &self.state.tc.info.id
    }
    pub fn limits(&self) -> &TurnLimits {
        &self.state.tc.info.options.limits
    }
    pub fn check_control(&self) -> Result<(), YourAiError> {
        self.state.tc.check_control()
    }
    pub async fn wait<T>(
        &mut self,
        future: impl std::future::Future<Output = Result<T, YourAiError>>,
    ) -> Result<T, YourAiError> {
        self.state
            .wait(future, self.state.op_timeout(), "business")
            .await
    }
    pub fn inputs(&mut self) -> InputExecutor<'_, 'turn> {
        InputExecutor {
            state: &mut self.state,
        }
    }
    pub fn permissions(&mut self) -> PermissionExecutor<'_, 'turn> {
        PermissionExecutor {
            state: &mut self.state,
        }
    }
    pub fn interaction(&mut self) -> InteractionExecutor<'_, 'turn> {
        InteractionExecutor {
            state: &mut self.state,
        }
    }
    pub async fn checkpoint(&mut self) -> Result<(), YourAiError> {
        self.state.checkpoint().await
    }
    /// Accept a business answer. Completion may request more work; hooks stay internal.
    pub async fn complete(&mut self, text: impl Into<String>) -> Result<Completion, YourAiError> {
        yourai_core::completion::complete(&mut CompletionExecutor { turn: self }, text.into()).await
    }
    /// Retain committed tool results, partial output and pending input on all exit paths.
    pub async fn finish(mut self, mut result: Result<(), YourAiError>) -> TurnResult {
        if result.is_ok() && self.accepted && !self.completed {
            result =
                Err(ErrorKind::Loop("complete the turn before reporting success".into()).into());
        }
        if let Err(error) = &result {
            if let Some(span) = &self.span {
                span.record_error(&error.to_string());
            }
            self.state.cleanup(error).await;
        }
        self.state.output.pending.extend(self.state.queued);
        match result {
            Ok(()) => Ok(self.state.output),
            Err(error) => Err(TurnFailure::new(error, self.state.output)),
        }
    }
}
impl ToolExecutor<'_, '_> {
    /// Execute a programmatic call through the same lifecycle as a model call.
    /// Binding and history ownership happen here, never in the scheduling program.
    pub async fn call(
        &mut self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<ToolOutput, YourAiError> {
        self.state.tc.check_control()?;
        if !self.state.unresolved.is_empty() {
            return Err(ErrorKind::Loop(
                "resolve pending calls before creating a programmatic tool call".into(),
            )
            .into());
        }
        let registry = self
            .state
            .tc
            .snap
            .tools
            .as_ref()
            .ok_or_else(|| ErrorKind::Config("tools not configured".into()))?;
        let handler = registry.resolve(name)?;
        if handler.name() != name || handler.definition().name.as_str() != name {
            return Err(
                ErrorKind::Config("tool handler name differs from its schema".into()).into(),
            );
        }
        let call = ToolCall {
            call_id: uuid::Uuid::new_v4().to_string(),
            fn_name: name.into(),
            fn_arguments: input,
            thought_signatures: None,
        };
        let history = self.state.history.clone();
        self.state
            .wait(
                history.append(vec![StoredMessage::new(ChatMessage::assistant(
                    MessageContent::from_tool_calls(vec![call.clone()]),
                ))]),
                self.state.op_timeout(),
                "history",
            )
            .await?;
        self.state.bound_tools.insert(name.into(), handler);
        self.state.call_ids.insert(call.call_id.clone());
        self.state.unresolved.push_back(call.clone());
        self.exec(&call.call_id).await
    }
    /// Only identities from the current model response can execute, exactly once.
    pub async fn exec(&mut self, call_id: &str) -> Result<ToolOutput, YourAiError> {
        let call = self
            .state
            .unresolved
            .iter()
            .find(|c| c.call_id == call_id)
            .cloned()
            .ok_or_else(|| ErrorKind::Tool {
                name: call_id.into(),
                message: "tool call is not pending in this turn".into(),
            })?;
        let binding = self
            .state
            .bound_tools
            .get(&call.fn_name)
            .cloned()
            .ok_or_else(|| ErrorKind::Tool {
                name: call.fn_name.clone(),
                message: "tool was not bound for this call".into(),
            })?;
        binding.exec(self, call).await
    }
    pub async fn exec_pending(&mut self) -> Result<Vec<ToolOutput>, YourAiError> {
        let mut results = vec![];
        while let Some(call) = self.state.unresolved.front().cloned() {
            self.state.tc.check_control()?;
            results.push(self.exec(&call.call_id).await?);
        }
        Ok(results)
    }
    pub async fn reject_pending(&mut self, reason: &str) -> Result<(), YourAiError> {
        let pending = self.state.unresolved.clone();
        let records = pending
            .iter()
            .map(|call| tools::result_record(call, &serde_json::json!({"error": reason}), true))
            .collect();
        let history = self.state.history.clone();
        self.state
            .wait(history.append(records), self.state.op_timeout(), "history")
            .await?;
        self.state.unresolved.clear();
        for call in pending {
            self.state.send(Out::ToolDone {
                id: call.call_id,
                name: call.fn_name,
                output: serde_json::json!({"error": reason}),
                is_error: true,
            })?;
        }
        Ok(())
    }
    pub fn pending(&self) -> Vec<ToolCall> {
        self.state.unresolved.iter().cloned().collect()
    }
}
impl yourai_core::model::ModelOperation for ModelExecutor<'_, '_> {
    fn exec_bound<'a>(
        &'a mut self,
        options: ModelOptions,
    ) -> BoxFuture<'a, Result<ModelOutput, YourAiError>> {
        Box::pin(async move {
            if !self.state.unresolved.is_empty() {
                return Err(ErrorKind::Loop(
                    "resolve pending tool calls before requesting another model response".into(),
                )
                .into());
            }
            self.state.next_model(&options).await
        })
    }
}
impl ModelExecutor<'_, '_> {
    pub async fn exec(&mut self) -> Result<ModelOutput, YourAiError> {
        self.exec_with(ModelOptions::default()).await
    }
    pub async fn exec_with(&mut self, options: ModelOptions) -> Result<ModelOutput, YourAiError> {
        // Fixed public entry: the step lifecycle (request build, streaming,
        // timeouts, retry accounting, StopFailure reporting) lives in the
        // framework operation, never in the caller.
        yourai_core::model::exec(self, options).await
    }
}
impl ContextExecutor<'_, '_> {
    pub fn messages(&self) -> Vec<ChatMessage> {
        self.state.history.messages()
    }
    pub async fn compact(
        &mut self,
        trigger: CompactionTrigger,
    ) -> Result<CompactionResult, YourAiError> {
        self.state.compact(trigger).await
    }
}

impl<'a> TurnExecution<'a> {
    pub async fn open(
        tc: TurnContext<'a>,
        mut config: ExecutionConfig,
    ) -> Result<Self, TurnFailure> {
        if let Some(input) = &tc.info.options.input {
            config.skill_ids = input.skill_ids.clone();
            config.memory_search_limit = input.memory_search_limit;
        }
        let history = tc
            .snap
            .context_manager
            .clone()
            .ok_or_else(|| ErrorKind::Config("execution requires ContextManager".into()))?;
        history.restore().await?;
        let model = tc.snap.model.clone();
        let span = tc
            .snap
            .observability
            .as_ref()
            .map(|o| o.span("execution.turn"));
        if let Some(span) = &span {
            span.record("turn_id", tc.info.id.as_str());
        }
        let state = ExecutionState {
            tc,
            config,
            history,
            model,
            output: TurnOutput::new(""),
            queued: VecDeque::new(),
            input_closed: false,
            model_calls: 0,
            stop_continuations: 0,
            repeated_tool: None,
            bound_tools: HashMap::new(),
            call_ids: HashSet::new(),
            unresolved: VecDeque::new(),
            tool_completion: None,
            tool_cleanup_deadline: None,
            partial_message: None,
            request_observation: None,
            deferred_context: vec![],
        };
        let mut execution = Self {
            state,
            accepted: false,
            completed: false,
            span,
        };
        match execution.state.initialize().await {
            Ok(accepted) => {
                execution.accepted = accepted;
                Ok(execution)
            }
            Err(error) => Err(execution
                .finish(Err(error))
                .await
                .expect_err("failed initialization")),
        }
    }
}

struct ExecutionState<'a> {
    tc: TurnContext<'a>,
    config: ExecutionConfig,
    history: Arc<dyn ContextManager>,
    model: Option<Arc<dyn ModelProvider>>,
    output: TurnOutput,
    queued: VecDeque<In>,
    input_closed: bool,
    model_calls: u32,
    stop_continuations: u32,
    repeated_tool: Option<(String, serde_json::Value, u32)>,
    unresolved: VecDeque<ToolCall>,
    bound_tools: HashMap<String, ToolBinding>,
    call_ids: HashSet<String>,
    deferred_context: Vec<String>,
    partial_message: Option<ChatMessage>,
    request_observation: Option<RequestObservation>,
    tool_completion: Option<(ToolCall, serde_json::Value, bool)>,
    tool_cleanup_deadline: Option<tokio::time::Instant>,
}

impl ExecutionState<'_> {
    async fn complete_candidate(&mut self, text: String) -> Result<(), YourAiError> {
        // Model.exec already committed its response. A program can also produce
        // a final business answer without calling a model; commit it once here.
        if text != self.output.text {
            let message = ChatMessage::assistant(text.clone());
            self.output.text = text.clone();
            self.partial_message = Some(message.clone());
            let history = self.history.clone();
            self.wait(
                history.append(vec![StoredMessage::new(message)]),
                self.op_timeout(),
                "history",
            )
            .await?;
            self.partial_message = None;
            self.send(Out::Message { text })?;
        }
        Ok(())
    }
    async fn initialize(&mut self) -> Result<bool, YourAiError> {
        self.tc.check_control()?;
        for message in self.history.messages() {
            self.call_ids.extend(
                message
                    .content
                    .tool_calls()
                    .into_iter()
                    .map(|call| call.call_id.clone()),
            );
        }
        // Core enqueues the first message before invoking us; no second inbox reader.
        let first = self
            .tc
            .inbox
            .try_recv()
            .map_err(|_| ErrorKind::Loop("missing initial input".into()))?;
        match &first {
            In::UserText { .. } => (),
            _ => {
                self.queued.push_back(first);
                return Err(ErrorKind::Config("Turn must start with UserText".into()).into());
            }
        };
        self.queued.push_back(first);
        if !self.accept_input(0, true).await? {
            return Ok(false);
        }
        Ok(true)
    }
    async fn next_model(&mut self, options: &ModelOptions) -> Result<ModelOutput, YourAiError> {
        let model = self
            .model
            .clone()
            .ok_or_else(|| ErrorKind::Config("model not configured".into()))?;
        let mut retries = 0;
        let mut overflow = 0;
        let mut force_compact = false;
        loop {
            self.checkpoint().await?;
            if force_compact {
                self.compact(CompactionTrigger::Overflow).await?;
                force_compact = false;
            }
            let attempt = self.model_step(retries + 1, options).await;
            let (message, calls) = match attempt {
                Ok(value) => value,
                Err((error, visible)) => {
                    if matches!(error, YourAiError::Aborted(_)) {
                        return Err(error);
                    }
                    if !visible {
                        match model.recovery(&error) {
                            ModelRecovery::Compact
                                if overflow < self.config.max_overflow_compactions =>
                            {
                                overflow += 1;
                                force_compact = true;
                                self.notice(
                                    Level::Warning,
                                    "Context overflow; compacting before retry",
                                )?;
                                continue;
                            }
                            ModelRecovery::Retry if retries < self.config.max_model_retries => {
                                let delay = crate::model::retry::Backoff {
                                    initial: self.config.retry_delay,
                                    max_without_headers: self.config.retry_max_delay,
                                }
                                .delay(
                                    retries,
                                    &error,
                                    model.retry_after(&error),
                                    (uuid::Uuid::new_v4().as_u128() % 100) as u32,
                                );
                                retries += 1;
                                self.send(Out::Retry {
                                    attempt: retries,
                                    max: self.config.max_model_retries,
                                    reason: model::retry_cause(&error),
                                    wait_ms: delay.as_millis().min(u64::MAX as u128) as u64,
                                })?;
                                // checked_add: an absurd server retry-after must not panic here.
                                self.wait(
                                    async {
                                        tokio::time::sleep(delay).await;
                                        Ok(())
                                    },
                                    self.config
                                        .operation_timeout
                                        .and_then(|op| delay.checked_add(op)),
                                    "retry",
                                )
                                .await?;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    let is_model_error =
                        matches!(&error, YourAiError::Error(ErrorKind::Model { .. }))
                            || matches!(
                                &error,
                                YourAiError::Error(ErrorKind::Provider { name, .. })
                                    if *name == crate::model::MODEL_NAME
                            );
                    if is_model_error {
                        self.stop_failure(&error).await;
                    }
                    if let Some((status, _)) = crate::model::failure::http_error(&error) {
                        self.notice(Level::Warning, format!("HTTP {status}: stopped after {} attempt(s) for this model step; {} model call(s) in this turn", retries + 1, self.model_calls))?;
                    }
                    return Err(error);
                }
            };
            let history = self.history.clone();
            if message
                .content
                .texts()
                .iter()
                .any(|text| !text.trim().is_empty())
            {
                self.repeated_tool = None;
            }
            let mut record = StoredMessage::new(message);
            record.model_response = true;
            record.request_observation = self.request_observation.take();
            self.wait(history.append(vec![record]), self.op_timeout(), "history")
                .await?;
            self.partial_message = None;
            self.unresolved = calls.into();
            self.send(Out::Message {
                text: self.output.text.clone(),
            })?;
            return Ok(ModelOutput {
                text: self.output.text.clone(),
                calls: self.unresolved.iter().cloned().collect(),
            });
        }
    }
    async fn finish(&mut self) -> Result<bool, YourAiError> {
        self.drain();
        if self.has_steer() || self.has_events() {
            return Ok(false);
        }
        let result = self
            .hook(HookEvent::Stop {
                stop_hook_active: self.stop_continuations > 0,
                last_assistant_message: Some(self.output.text.clone()),
            })
            .await?;
        self.apply_common(&result)?;
        let feedback = hooks::feedback(&result);
        let extra = hooks::additional(&result);
        if !result.common.blocking_errors.is_empty() {
            if self.stop_continuations >= self.config.max_stop_continuations {
                return Err(ErrorKind::Loop("Stop continuation limit reached".into()).into());
            }
            self.stop_continuations += 1;
            self.add_context(&[extra, feedback].concat()).await?;
            return Ok(false);
        }
        self.add_context(&extra).await?;
        self.drain();
        if !self.has_steer() && !self.has_events() {
            return Ok(true);
        }
        Ok(false)
    }
    async fn compact(
        &mut self,
        trigger: CompactionTrigger,
    ) -> Result<CompactionResult, YourAiError> {
        let mut request = CompactionRequest::new(trigger);
        request.deadline = self.deadline(self.op_timeout());
        request.tools = self
            .tc
            .snap
            .tools
            .as_ref()
            .map(|r| r.definitions())
            .unwrap_or_default();
        let calls = request.calls.clone();
        let usage = request.usage.clone();
        let cancel = self.tc.cancel.child_token();
        let _guard = cancel.clone().drop_guard();
        let history = self.history.clone();
        let result = self
            .wait(
                crate::context::compact(
                    history.as_ref(),
                    request,
                    &ContextExecution::from_snapshot(
                        &self.tc.snap,
                        history.session_id(),
                        self.tc.info.options.session.as_deref(),
                    )?,
                    &cancel,
                    self.tc
                        .info
                        .options
                        .limits
                        .hook_timeout
                        .or(self.config.hook_timeout),
                ),
                self.op_timeout(),
                "compact",
            )
            .await;
        self.model_calls += calls.load(std::sync::atomic::Ordering::Acquire);
        let usages = usage.lock().unwrap().clone();
        for u in usages {
            let input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
            let output = u.completion_tokens.unwrap_or(0).max(0) as u64;
            if u.prompt_tokens.is_some()
                || u.completion_tokens.is_some()
                || u.total_tokens.is_some()
            {
                self.record_usage(Usage {
                    input_tokens: input,
                    output_tokens: output,
                    total_tokens: u
                        .total_tokens
                        .map(|n| n.max(0) as u64)
                        .unwrap_or(input + output),
                })
                .await?;
            }
        }
        let result = result?;
        for notice in &result.notices {
            self.notice(Level::Info, notice)?;
        }
        if let Some(reason) = &result.stop_reason {
            return Err(AbortReason::HookStopped(reason.clone()).into());
        }
        if trigger == CompactionTrigger::Overflow
            && (result.action == CompactAction::Unchanged
                || result.tokens_after >= result.tokens_before)
        {
            return Err(ErrorKind::Loop("overflow compaction made no progress".into()).into());
        }
        if result.action != CompactAction::Unchanged {
            self.notice(
                Level::Info,
                format!(
                    "Context {:?}: {} -> {} tokens",
                    result.action, result.tokens_before, result.tokens_after
                ),
            )?;
        }
        Ok(result)
    }
}
