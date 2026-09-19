//! DefaultLoop owns one Turn; session lifecycle and extensions remain outside it.
//! All state is local to run_turn, so one instance can serve independent sessions.
mod control;
mod hooks;
mod interaction;
mod model;
mod tools;

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::Arc,
    time::Duration,
};
use yourai_core::{model::ModelRecovery, prelude::*};

/// Policy defaults, not additional Providers. TurnLimits can impose stricter limits.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    pub system_prompt: Option<String>,
    /// Explicitly selected skills; listing a skill does not activate it.
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
    pub max_model_calls: u32,
    pub max_tool_calls: u32,
    pub max_model_retries: u32,
    pub max_overflow_compactions: u32,
    pub max_stop_continuations: u32,
    pub max_permission_rechecks: u32,
    pub retry_delay: Duration,
    pub operation_timeout: Duration,
    pub approval_timeout: Duration,
    pub hook_timeout: Duration,
    pub cleanup_timeout: Duration,
}
impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            system_prompt: None,
            skill_ids: vec![],
            memory_search_limit: 0,
            max_model_calls: 64,
            max_tool_calls: 256,
            max_model_retries: 2,
            max_overflow_compactions: 1,
            max_stop_continuations: 3,
            max_permission_rechecks: 1,
            retry_delay: Duration::from_millis(250),
            operation_timeout: Duration::from_secs(120),
            approval_timeout: Duration::from_secs(300),
            hook_timeout: Duration::from_secs(30),
            cleanup_timeout: Duration::from_secs(5),
        }
    }
}
#[derive(Debug, Clone, Default)]
pub struct DefaultLoop {
    config: LoopConfig,
}
impl DefaultLoop {
    pub fn new(config: LoopConfig) -> Self {
        Self { config }
    }
    pub fn config(&self) -> &LoopConfig {
        &self.config
    }
}

impl AgentLoop for DefaultLoop {
    fn request_system<'a>(
        &'a self,
        providers: &'a ProviderSnapshot,
        session: Option<&'a SessionContext>,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        Box::pin(async move {
            let mut system = self.config.system_prompt.clone().unwrap_or_default();
            if let Some(session) = session {
                for text in session.instructions.values() {
                    system.push('\n');
                    system.push_str(text);
                }
            }
            if !self.config.skill_ids.is_empty() {
                let skills = providers.skills.as_ref().ok_or_else(|| {
                    ErrorKind::Config("selected skills require SkillProvider".into())
                })?;
                for id in &self.config.skill_ids {
                    system.push('\n');
                    system.push_str(&skills.load(id).await?.instructions);
                }
            }
            Ok(system)
        })
    }

    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let history =
                tc.snap.context_manager.clone().ok_or_else(|| {
                    ErrorKind::Config("DefaultLoop requires ContextManager".into())
                })?;
            history.restore().await?;
            let model =
                tc.snap.model.clone().ok_or_else(|| {
                    ErrorKind::Config("DefaultLoop requires ModelProvider".into())
                })?;
            let span = tc
                .snap
                .observability
                .as_ref()
                .map(|o| o.span("default_loop.turn"));
            if let Some(span) = &span {
                span.record("turn_id", tc.info.id.as_str());
            }
            let mut state = State {
                tc,
                config: &self.config,
                history,
                model,
                output: TurnOutput::new(""),
                queued: VecDeque::new(),
                input_closed: false,
                model_calls: 0,
                tool_calls: 0,
                stop_continuations: 0,
                bound_tools: HashMap::new(),
                call_ids: HashSet::new(),
                unresolved: VecDeque::new(),
                tool_completion: None,
                partial_message: None,
                request_observation: None,
                deferred_context: vec![],
                system: self.config.system_prompt.clone().unwrap_or_default(),
            };
            let result = state.run().await;
            if let Err(error) = &result {
                if let Some(span) = &span {
                    span.record_error(&error.to_string());
                }
                state.cleanup(error).await;
            }
            state.output.pending.extend(state.queued);
            match result {
                Ok(()) => Ok(state.output),
                Err(error) => Err(TurnFailure::new(error, state.output)),
            }
        })
    }
}

struct State<'a> {
    tc: TurnContext<'a>,
    config: &'a LoopConfig,
    history: Arc<dyn ContextManager>,
    model: Arc<dyn ModelProvider>,
    output: TurnOutput,
    queued: VecDeque<In>,
    input_closed: bool,
    model_calls: u32,
    tool_calls: u32,
    stop_continuations: u32,
    unresolved: VecDeque<ToolCall>,
    system: String,
    bound_tools: HashMap<String, Arc<dyn ToolHandler>>,
    call_ids: HashSet<String>,
    deferred_context: Vec<String>,
    partial_message: Option<ChatMessage>,
    request_observation: Option<RequestObservation>,
    tool_completion: Option<(ToolCall, serde_json::Value, bool)>,
}

impl State<'_> {
    async fn run(&mut self) -> Result<(), YourAiError> {
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
        let prompt = match &first {
            In::UserText { text, .. } => text.clone(),
            _ => {
                self.queued.push_back(first);
                return Err(ErrorKind::Config("Turn must start with UserText".into()).into());
            }
        };
        self.queued.push_back(first);
        if !self.accept_input(0).await? {
            return Ok(());
        }
        self.prepare_system(&prompt).await?;
        let mut retries = 0;
        let mut overflow = 0;
        let mut force_compact = false;
        loop {
            self.checkpoint().await?;
            if force_compact {
                self.compact(CompactionTrigger::Overflow).await?;
                force_compact = false;
            }
            self.check_model_budget()?;
            let attempt = self.model_step().await;
            let (message, calls) = match attempt {
                Ok(value) => {
                    retries = 0;
                    overflow = 0;
                    value
                }
                Err((error, visible)) => {
                    if matches!(error, YourAiError::Aborted(_)) {
                        return Err(error);
                    }
                    if !visible {
                        match self.model.recovery(&error) {
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
                                let delay = self
                                    .config
                                    .retry_delay
                                    .saturating_mul(1u32 << retries.min(10));
                                retries += 1;
                                self.notice(Level::Warning, "Model request failed; retrying")?;
                                self.wait(
                                    async {
                                        tokio::time::sleep(delay).await;
                                        Ok(())
                                    },
                                    None,
                                    "retry",
                                )
                                .await?;
                                continue;
                            }
                            _ => {}
                        }
                    }
                    if matches!(
                        &error,
                        YourAiError::Error(
                            ErrorKind::Model { .. } | ErrorKind::Provider { name: "model", .. }
                        )
                    ) {
                        self.stop_failure(&error).await;
                    }
                    return Err(error);
                }
            };
            let history = self.history.clone();
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
            if !self.unresolved.is_empty() {
                while let Some(call) = self.unresolved.front().cloned() {
                    self.tc.check_control()?;
                    self.execute_tool(call).await?;
                }
                continue;
            }
            self.drain();
            if self.has_steer() || self.has_events() {
                continue;
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
                continue;
            }
            self.add_context(&extra).await?;
            self.drain();
            if !self.has_steer() && !self.has_events() {
                return Ok(());
            }
        }
    }

    async fn prepare_system(&mut self, prompt: &str) -> Result<(), YourAiError> {
        let snapshot = self.tc.snap.clone();
        let session = self.tc.info.options.session.clone();
        self.system = self
            .wait(
                snapshot
                    .agent_loop
                    .request_system(&snapshot, session.as_deref()),
                self.op_timeout(),
                "system",
            )
            .await?;
        if self.config.memory_search_limit > 0 {
            if let Some(memory) = self.tc.snap.memory.clone() {
                let entries = self
                    .wait(
                        memory.search(prompt, self.config.memory_search_limit),
                        self.op_timeout(),
                        "memory",
                    )
                    .await?;
                // Retrieved content is context data, never a new system instruction.
                let context: Vec<_> = entries
                    .into_iter()
                    .map(|e| format!("Memory [{}]: {}", e.key, e.value))
                    .collect();
                self.add_context(&context).await?;
            }
        }
        Ok(())
    }
    fn check_model_budget(&self) -> Result<(), YourAiError> {
        self.tc.check_control()?;
        let max = self
            .tc
            .info
            .options
            .limits
            .max_model_calls
            .unwrap_or(self.config.max_model_calls)
            .min(self.config.max_model_calls);
        if self.model_calls >= max {
            return Err(AbortReason::LimitReached(TurnLimit::ModelCalls).into());
        }
        Ok(())
    }
    async fn compact(&mut self, trigger: CompactionTrigger) -> Result<(), YourAiError> {
        let mut request = CompactionRequest::new(trigger);
        request.deadline = Some(self.deadline(self.op_timeout()));
        request.system = Some(self.system.clone());
        request.tools = self
            .tc
            .snap
            .tools
            .as_ref()
            .map(|r| r.definitions())
            .unwrap_or_default();
        let max = self
            .tc
            .info
            .options
            .limits
            .max_model_calls
            .unwrap_or(self.config.max_model_calls)
            .min(self.config.max_model_calls);
        request.max_model_calls = max.saturating_sub(self.model_calls);
        let calls = request.calls.clone();
        let usage = request.usage.clone();
        let cancel = self.tc.cancel.child_token();
        let _guard = cancel.clone().drop_guard();
        let history = self.history.clone();
        let result = self
            .wait(
                history.compact(
                    request,
                    &ContextExecution::from_snapshot(
                        &self.tc.snap,
                        history.session_id(),
                        self.tc.info.options.session.as_deref(),
                    )?,
                    &cancel,
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
        for notice in result.notices {
            self.notice(Level::Info, notice)?;
        }
        if let Some(reason) = result.stop_reason {
            return Err(AbortReason::HookStopped(reason).into());
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
        Ok(())
    }
}
