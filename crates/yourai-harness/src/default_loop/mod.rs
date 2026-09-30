//! DefaultLoop owns one Turn; session lifecycle and extensions remain outside it.
//! All state is local to run_turn, so one instance can serve independent sessions.
mod admission;
mod attachment;
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

// Verbatim from opencode `packages/core/src/session/runner/max-steps.ts`.
const MAX_STEPS_PROMPT: &str = r#"CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY."#;

/// Defensive bound on forced-final iterations that still return tool calls.
const MAX_FORCED_FINAL_CONTINUATIONS: u32 = 3;

/// Image attachment policy, mirroring opencode `attachment.image.*`
/// (`packages/opencode/src/image/image.ts`). Over-sized images are resized
/// (Lanczos3, PNG then JPEG at descending qualities) instead of rejected;
/// only an image that cannot be brought within limits fails.
#[derive(Debug, Clone)]
pub struct AttachmentImageConfig {
    pub auto_resize: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// Ceiling on the base64 payload length, matching the provider-side
    /// limit the number applies to (opencode: 5 MiB base64).
    pub max_base64_bytes: usize,
}
impl Default for AttachmentImageConfig {
    fn default() -> Self {
        Self {
            auto_resize: true,
            max_width: 2000,
            max_height: 2000,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Policy defaults, not additional Providers. TurnLimits can impose stricter limits.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Explicitly selected skills; listing a skill does not activate it.
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
    pub memory_max_chars: usize,
    /// OpenCode-compatible agentic iteration cap. None means unlimited.
    pub steps: Option<u32>,
    /// OpenCode-compatible attachment image normalization policy.
    pub attachment_image: AttachmentImageConfig,
    /// Char cap for text files attached by reference (before line-window
    /// selection the cap applies to the selected window). Directories and
    /// binary media are unaffected.
    pub attachment_text_max_chars: usize,
    pub max_model_retries: u32,
    pub max_overflow_compactions: u32,
    pub max_stop_continuations: u32,
    pub max_permission_rechecks: u32,
    pub retry_delay: Duration,
    pub retry_max_delay: Duration,
    /// Optional bound for generic provider operations; no implicit turn deadline.
    pub operation_timeout: Option<Duration>,
    /// Optional host tool bound. Built-in tools own their default timeouts.
    pub tool_timeout: Option<Duration>,
    pub approval_timeout: Option<Duration>,
    pub hook_timeout: Option<Duration>,
    /// Optional bound for durable cleanup, independent of the cancelled turn.
    pub cleanup_timeout: Option<Duration>,
    /// Grace for tools to settle after cancellation, shared with failure reporting.
    pub tool_cleanup_timeout: Duration,
}
impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            skill_ids: vec![],
            memory_search_limit: 0,
            memory_max_chars: 8000,
            steps: None,
            attachment_image: AttachmentImageConfig::default(),
            attachment_text_max_chars: 50_000,
            max_model_retries: 5,
            max_overflow_compactions: 1,
            max_stop_continuations: 3,
            max_permission_rechecks: 1,
            retry_delay: Duration::from_secs(2),
            retry_max_delay: Duration::from_secs(30),
            operation_timeout: None,
            tool_timeout: None,
            approval_timeout: None,
            hook_timeout: None,
            cleanup_timeout: None,
            tool_cleanup_timeout: Duration::from_millis(250),
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
                forced_final: false,
                forced_final_continuations: 0,
                step: 0,
                model_calls: 0,
                stop_continuations: 0,
                bound_tools: HashMap::new(),
                call_ids: HashSet::new(),
                unresolved: VecDeque::new(),
                tool_completion: None,
                tool_cleanup_deadline: None,
                partial_message: None,
                request_observation: None,
                deferred_context: vec![],
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
    forced_final: bool,
    /// Bounded retries after a forced-final model still returned tool calls
    /// (opencode keeps looping; we cap it defensively).
    forced_final_continuations: u32,
    step: u32,
    model_calls: u32,
    stop_continuations: u32,
    unresolved: VecDeque<ToolCall>,
    bound_tools: HashMap<String, Arc<dyn ToolHandler>>,
    call_ids: HashSet<String>,
    deferred_context: Vec<String>,
    partial_message: Option<ChatMessage>,
    request_observation: Option<RequestObservation>,
    tool_completion: Option<(ToolCall, serde_json::Value, bool)>,
    tool_cleanup_deadline: Option<tokio::time::Instant>,
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
        match &first {
            In::UserText { .. } => (),
            _ => {
                self.queued.push_back(first);
                return Err(ErrorKind::Config("Turn must start with UserText".into()).into());
            }
        };
        self.queued.push_back(first);
        if !self.accept_input(0, true).await? {
            return Ok(());
        }
        if self.effective_steps() == Some(0) {
            return Err(ErrorKind::Config("steps must be a positive integer".into()).into());
        }
        let mut retries = 0;
        let mut overflow = 0;
        let mut force_compact = false;
        let mut new_step = true;
        loop {
            self.checkpoint().await?;
            if force_compact {
                self.compact(CompactionTrigger::Overflow).await?;
                force_compact = false;
            }
            if new_step {
                self.step += 1;
                if !self.forced_final && self.effective_steps().is_some_and(|max| self.step >= max)
                {
                    // The prompt itself is injected per request in read_model
                    // (assistant-role prefill + tool_choice none), matching
                    // opencode's runner; only the latch lives here.
                    self.forced_final = true;
                }
            }
            let attempt = self.model_step(retries + 1).await;
            let (message, calls) = match attempt {
                Ok(value) => {
                    retries = 0;
                    overflow = 0;
                    new_step = true;
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
                                new_step = false;
                                self.notice(
                                    Level::Warning,
                                    "Context overflow; compacting before retry",
                                )?;
                                continue;
                            }
                            ModelRecovery::Retry if retries < self.config.max_model_retries => {
                                new_step = false;
                                let delay = crate::model::retry::Backoff {
                                    initial: self.config.retry_delay,
                                    max_without_headers: self.config.retry_max_delay,
                                }
                                .delay(
                                    retries,
                                    &error,
                                    self.model.retry_after(&error),
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
                    if matches!(
                        &error,
                        YourAiError::Error(
                            ErrorKind::Model { .. } | ErrorKind::Provider { name: "model", .. }
                        )
                    ) {
                        self.stop_failure(&error).await;
                    }
                    if let Some((status, _)) = error.model_http_error() {
                        self.notice(Level::Warning, format!("HTTP {status}: stopped after {} attempt(s) for this model step; {} model call(s) in this turn", retries + 1, self.model_calls))?;
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
            if self.forced_final {
                if !self.unresolved.is_empty() {
                    // opencode `failUnsettledTools`: record explicit failure
                    // results so the model can see why its calls were refused
                    // and produce the required text-only summary. A provider
                    // honoring tool_choice=none never reaches this; the bound
                    // only guards a misbehaving one.
                    if self.forced_final_continuations >= MAX_FORCED_FINAL_CONTINUATIONS {
                        self.notice(
                            Level::Warning,
                            "Model requested tools after maximum agent steps; they were not executed.",
                        )?;
                        // Keep persisted tool-call/result pairs complete for the next turn.
                        self.cleanup(
                            &ErrorKind::Loop("tools are disabled after maximum agent steps".into())
                                .into(),
                        )
                        .await;
                        return Ok(());
                    }
                    self.forced_final_continuations += 1;
                    let failed = std::mem::take(&mut self.unresolved);
                    let mut records = vec![];
                    for call in &failed {
                        let output = serde_json::json!({
                            "error": "Tools are disabled after the maximum agent steps"
                        });
                        records.push(tools::result_record(call, &output, true));
                        self.send(Out::ToolDone {
                            id: call.call_id.clone(),
                            name: call.fn_name.clone(),
                            output,
                            is_error: true,
                        })?;
                    }
                    let history = self.history.clone();
                    self.wait(history.append(records), self.op_timeout(), "history")
                        .await?;
                    self.notice(
                        Level::Warning,
                        "Tools are disabled after the maximum agent steps",
                    )?;
                    continue;
                }
                return Ok(());
            }
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

    /// Per-turn options may only tighten the configured agent step cap.
    fn effective_steps(&self) -> Option<u32> {
        let config = self.config.steps;
        match self.tc.info.options.limits.steps {
            Some(limit) => Some(config.map_or(limit, |configured| configured.min(limit))),
            None => config,
        }
    }
    async fn compact(&mut self, trigger: CompactionTrigger) -> Result<(), YourAiError> {
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
