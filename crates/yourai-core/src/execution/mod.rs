//! Public business execution, independent of any scheduling strategy.
//! AgentLoop implementations call these operations without dispatching hooks.
mod admission;
mod attachment;
mod config;
mod control;
mod hooks;

use crate::prelude::*;
pub use config::{AttachmentImageConfig, ExecutionConfig};
use std::{
    collections::{HashSet, VecDeque},
    sync::Arc,
};

pub use crate::completion::Completion;
pub use crate::model::{ModelOptions, ModelOutput};

/// One turn owns its state and fixed execution templates.
pub struct Turn<'a> {
    pub(crate) tc: TurnContext<'a>,
    pub(crate) config: ExecutionConfig,
    pub(crate) history: Arc<dyn ContextManager>,
    pub(crate) model: Option<crate::model::Model>,
    pub(crate) output: TurnOutput,
    pub(crate) queued: VecDeque<In>,
    pub(crate) input_closed: bool,
    pub(crate) model_calls: u32,
    pub(crate) stop_continuations: u32,
    pub(crate) repeated_tool: Option<(String, serde_json::Value, u32)>,
    pub(crate) calls: VecDeque<CallRecord>,
    pub(crate) call_ids: HashSet<String>,
    pub(crate) deferred_context: Vec<StoredMessage>,
    pub(crate) partial_message: Option<StoredMessage>,
    pub(crate) accepted: bool,
    pub(crate) completed: bool,
    pub(crate) span: Option<Arc<dyn crate::observability::Span>>,
}

#[derive(Clone)]
pub(crate) enum CallState {
    Pending,
    Running,
    Observed {
        output: serde_json::Value,
        is_error: bool,
    },
}
#[derive(Clone)]
pub(crate) struct CallRecord {
    pub call: ToolCall,
    pub tool: Option<Tool>,
    pub state: CallState,
    pub result_id: String,
}
impl CallRecord {
    pub fn result_record(&self, output: &serde_json::Value) -> StoredMessage {
        let mut record =
            StoredMessage::new(ToolResponse::from_tool_call(&self.call, output.to_string()).into());
        record.id = self.result_id.clone();
        record
    }
}

impl<'a> Turn<'a> {
    pub fn model(&self) -> Result<crate::model::Model, YourAiError> {
        self.model
            .clone()
            .ok_or_else(|| ErrorKind::Config("model not configured".into()).into())
    }
    pub fn tool(&self, call_id: &str) -> Result<Tool, YourAiError> {
        self.calls
            .iter()
            .find(|record| record.call.call_id == call_id)
            .and_then(|record| record.tool.clone())
            .ok_or_else(|| {
                ErrorKind::Tool {
                    name: call_id.into(),
                    message: "tool call is not bound in this turn".into(),
                }
                .into()
            })
    }
    pub fn pending_tools(&self) -> Vec<ToolCall> {
        self.calls
            .iter()
            .map(|record| record.call.clone())
            .collect()
    }
    /// Enqueue a programmatic call. The frozen Tool owns its execution.
    pub async fn enqueue(
        &mut self,
        name: &str,
        input: serde_json::Value,
    ) -> Result<ToolCall, YourAiError> {
        self.ensure_active()?;
        if !self.calls.is_empty() {
            return Err(
                ErrorKind::Loop("resolve pending calls before enqueueing a tool".into()).into(),
            );
        }
        let tool = self
            .tc
            .snap
            .tools
            .as_ref()
            .ok_or_else(|| ErrorKind::Config("tools not configured".into()))?
            .resolve(name)?;
        let call = ToolCall {
            call_id: uuid::Uuid::new_v4().to_string(),
            fn_name: name.into(),
            fn_arguments: input,
            thought_signatures: None,
        };
        let record = StoredMessage::new(ChatMessage::assistant(MessageContent::from_tool_calls(
            vec![call.clone()],
        )));
        self.partial_message = Some(record.clone());
        self.register_calls(vec![call.clone()], &[tool]);
        let history = self.history.clone();
        self.wait_operation(history.append(vec![record]), self.op_timeout(), "history")
            .await?;
        self.partial_message = None;
        Ok(call)
    }
    pub(crate) fn register_calls(&mut self, calls: Vec<ToolCall>, tools: &[Tool]) {
        for call in calls {
            self.call_ids.insert(call.call_id.clone());
            let tool = tools
                .iter()
                .find(|tool| tool.name() == call.fn_name)
                .cloned();
            self.calls.push_back(CallRecord {
                call,
                tool,
                state: CallState::Pending,
                result_id: uuid::Uuid::new_v4().to_string(),
            });
        }
    }
    pub async fn reject_pending_tools(&mut self, reason: &str) -> Result<(), YourAiError> {
        self.ensure_active()?;
        if self
            .calls
            .iter()
            .any(|record| !matches!(record.state, CallState::Pending))
        {
            return Err(
                ErrorKind::Loop("cannot reject a tool that has already started".into()).into(),
            );
        }
        let output = serde_json::json!({"error":reason});
        for record in &mut self.calls {
            record.state = CallState::Observed {
                output: output.clone(),
                is_error: true,
            };
        }
        let records = self
            .calls
            .iter()
            .map(|record| record.result_record(&output))
            .collect();
        let history = self.history.clone();
        self.wait_operation(history.append(records), self.op_timeout(), "history")
            .await?;
        for record in self.calls.drain(..).collect::<Vec<_>>() {
            self.send(Out::ToolDone {
                id: record.call.call_id,
                name: record.call.fn_name,
                output: output.clone(),
                is_error: true,
            })?;
        }
        Ok(())
    }
    pub(crate) fn observe_tool_result(
        &mut self,
        call: &ToolCall,
        output: serde_json::Value,
        is_error: bool,
    ) {
        let record = self
            .calls
            .iter_mut()
            .find(|record| record.call.call_id == call.call_id)
            .expect("claimed tool call");
        record.call = call.clone();
        record.state = CallState::Observed { output, is_error };
    }
    pub(crate) fn ensure_active(&self) -> Result<(), YourAiError> {
        if self.completed {
            return Err(ErrorKind::Loop("turn is already complete".into()).into());
        }
        if self.partial_message.is_some() {
            return Err(ErrorKind::Loop(
                "history submission is unconfirmed; finish the turn before continuing".into(),
            )
            .into());
        }
        self.tc.check_control()
    }
    pub(crate) fn defer_context(&mut self, contexts: Vec<String>) {
        if !contexts.is_empty() {
            self.deferred_context
                .push(StoredMessage::runtime_context(format!(
                    "[Runtime context]\n{}",
                    contexts.join("\n")
                )));
        }
    }

    pub fn input_accepted(&self) -> bool {
        self.accepted
    }

    pub fn text(&self) -> &str {
        &self.output.text
    }

    pub fn turn_id(&self) -> &TurnId {
        &self.tc.info.id
    }

    pub fn limits(&self) -> &TurnLimits {
        &self.tc.info.options.limits
    }

    pub fn check_control(&self) -> Result<(), YourAiError> {
        self.tc.check_control()
    }

    pub async fn wait<T>(
        &mut self,
        future: impl std::future::Future<Output = Result<T, YourAiError>>,
    ) -> Result<T, YourAiError> {
        self.wait_operation(future, self.op_timeout(), "business")
            .await
    }

    pub async fn complete(&mut self, text: impl Into<String>) -> Result<Completion, YourAiError> {
        self.ensure_active()?;
        if !self.calls.is_empty() {
            return Err(
                ErrorKind::Loop("cannot complete with unresolved tool calls".into()).into(),
            );
        }
        self.complete_candidate(text.into()).await?;
        let completion = if self.check_completion().await? {
            Completion::Completed
        } else {
            Completion::NeedsMoreWork
        };
        self.completed = completion == Completion::Completed;
        Ok(completion)
    }

    pub async fn finish(mut self, mut result: Result<(), YourAiError>) -> TurnResult {
        if result.is_ok()
            && self.accepted
            && (!self.completed
                || !self.calls.is_empty()
                || self.partial_message.is_some()
                || !self.deferred_context.is_empty())
        {
            result =
                Err(ErrorKind::Loop("complete the turn before reporting success".into()).into());
        }
        if let Err(error) = &result {
            if let Some(span) = &self.span {
                span.record_error(&error.to_string());
            }
            self.cleanup(error).await;
        }
        self.output.pending.extend(self.queued);
        match result {
            Ok(()) => Ok(self.output),
            Err(error) => Err(TurnFailure::new(error, self.output)),
        }
    }

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
        let model = tc.snap.model.clone().map(crate::model::Model::new);
        let span = tc
            .snap
            .observability
            .as_ref()
            .map(|o| o.span("execution.turn"));
        if let Some(span) = &span {
            span.record("turn_id", tc.info.id.as_str());
        }
        let mut execution = Self {
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
            call_ids: HashSet::new(),
            calls: VecDeque::new(),
            partial_message: None,
            deferred_context: vec![],
            accepted: false,
            completed: false,
            span,
        };
        match execution.initialize().await {
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

    pub async fn accept_input(&mut self, mut input: In) -> Result<bool, YourAiError> {
        self.ensure_active()?;
        if !matches!(input, In::UserText { .. }) {
            return Err(ErrorKind::Config("input admission requires UserText".into()).into());
        }
        input.ensure_id();
        let index = self.queued.len();
        self.queued.push_back(input);
        self.accept_queued_input(index, false).await
    }

    pub fn messages(&self) -> Vec<ChatMessage> {
        self.history.messages()
    }

    async fn complete_candidate(&mut self, text: String) -> Result<(), YourAiError> {
        // Model.exec already committed its response. A program can also produce
        // a final business answer without calling a model; commit it once here.
        if text != self.output.text {
            let message = ChatMessage::assistant(text.clone());
            self.output.text = text.clone();
            let record = StoredMessage::new(message);
            self.partial_message = Some(record.clone());
            let history = self.history.clone();
            self.wait_operation(history.append(vec![record]), self.op_timeout(), "history")
                .await?;
            self.partial_message = None;
            self.send(Out::Message { text })?;
        }
        Ok(())
    }
    async fn initialize(&mut self) -> Result<bool, YourAiError> {
        self.tc.check_control()?;
        // Own the first input before any wait can route later inputs into the queue.
        let mut first = self
            .tc
            .inbox
            .try_recv()
            .map_err(|_| ErrorKind::Loop("missing initial input".into()))?;
        if !matches!(first, In::UserText { .. }) {
            self.queued.push_back(first);
            return Err(ErrorKind::Config("Turn must start with UserText".into()).into());
        }
        first.ensure_id();
        self.queued.push_back(first);
        let history = self.history.clone();
        self.wait_operation(history.restore(), self.op_timeout(), "history")
            .await?;
        for message in self.history.messages() {
            self.call_ids.extend(
                message
                    .content
                    .tool_calls()
                    .into_iter()
                    .map(|call| call.call_id.clone()),
            );
        }
        self.accept_queued_input(0, true).await
    }

    async fn check_completion(&mut self) -> Result<bool, YourAiError> {
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
        let feedback = result.blocking_messages();
        let extra = result.additional_contexts().to_vec();
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
    pub async fn compact(
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
        let execution =
            Compactor::from_snapshot(&self.tc.snap, self.tc.info.options.session.as_deref())?;
        let hook_timeout = self
            .tc
            .info
            .options
            .limits
            .hook_timeout
            .or(self.config.hook_timeout);
        self.ensure_active()?;
        let future = execution.exec(request, &cancel, hook_timeout);
        tokio::pin!(future);
        let result = loop {
            tokio::select! {
                biased;
                result = &mut future => break result,
                _ = self.tc.cancel.cancelled() => { cancel.cancel(); break future.await; }
                _ = self.tc.outbox.closed() => { cancel.cancel(); break future.await; }
                input = self.tc.inbox.recv(), if !self.input_closed => match input { Some(input) => self.route(input), None => self.input_closed = true },
            }
        };
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
