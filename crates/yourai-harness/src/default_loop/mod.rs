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

// Verbatim from opencode's `session/prompt/max-steps.txt`: injected on the final
// agentic step to force a text-only summary once the step cap is reached.
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

/// OpenCode caps agentic iterations at 1000 via `streamText`'s `stopWhen`
/// (`steps.length >= 1000`). `effective_steps` uses this as the safety-net
/// default when no explicit `steps` is configured; an explicit cap is honoured.
const MAX_AGENT_STEPS: u32 = 1000;

/// OpenCode `DOOM_LOOP_THRESHOLD`: when the same tool is invoked with identical
/// input three times in a row, it routes through `permission.ask` so a runaway
/// loop can be broken. `doom_loop_check` mirrors that gate.
const DOOM_LOOP_THRESHOLD: u32 = 3;

/// Policy defaults, not additional Providers. TurnLimits can impose stricter limits.
///
/// Timeouts follow OpenCode's model: only `model_header_timeout`,
/// `model_chunk_timeout` and `tool_timeout` have finite defaults. All other
/// timeouts default to `None` (unlimited), matching OpenCode which has no
/// operation/approval/hook/cleanup deadline.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Explicitly selected skills; listing a skill does not activate it.
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
    pub memory_max_chars: usize,
    /// OpenCode-compatible agentic iteration cap. None means unlimited up to the
    /// `MAX_AGENT_STEPS` (1000) hard ceiling imposed by `effective_steps`.
    pub steps: Option<u32>,
    /// Application-level retry cap. OpenCode splits retries across the AI SDK
    /// (`maxRetries: 3` on `streamText`) and an uncapped processor layer with
    /// back-off; YourAI has a single layer, capped at 3 with OpenCode-aligned
    /// back-off (see `retry_delay` / `retry_max_delay`).
    pub max_model_retries: u32,
    pub max_overflow_compactions: u32,
    pub max_stop_continuations: u32,
    pub max_permission_rechecks: u32,
    /// OpenCode `SessionRetry` initial back-off: 2s (`RETRY_INITIAL_DELAY = 2000`).
    pub retry_delay: Duration,
    /// OpenCode `SessionRetry` back-off ceiling without `Retry-After` headers:
    /// 30s (`RETRY_MAX_DELAY_NO_HEADERS = 30_000`).
    pub retry_max_delay: Duration,
    /// OpenCode has no operation timeout; `None` = unlimited.
    pub operation_timeout: Option<Duration>,
    /// OpenCode `headerTimeout` equivalent (default 300s).
    pub model_header_timeout: Duration,
    /// OpenCode `chunkTimeout` equivalent (default 300s).
    pub model_chunk_timeout: Duration,
    /// OpenCode `experimental.tool_timeout` equivalent (default 600s).
    pub tool_timeout: Duration,
    /// OpenCode has no approval timeout; `None` = unlimited.
    pub approval_timeout: Option<Duration>,
    /// OpenCode has no hook timeout; `None` = unlimited.
    pub hook_timeout: Option<Duration>,
    /// OpenCode has no cleanup timeout; `None` = unlimited.
    pub cleanup_timeout: Option<Duration>,
}
impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            skill_ids: vec![],
            memory_search_limit: 0,
            memory_max_chars: 8000,
            steps: None,
            max_model_retries: 3,
            max_overflow_compactions: 1,
            max_stop_continuations: 3,
            max_permission_rechecks: 1,
            retry_delay: Duration::from_secs(2),
            retry_max_delay: Duration::from_secs(30),
            operation_timeout: None,
            model_header_timeout: Duration::from_secs(300),
            model_chunk_timeout: Duration::from_secs(300),
            tool_timeout: Duration::from_secs(600),
            approval_timeout: None,
            hook_timeout: None,
            cleanup_timeout: None,
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
                step: 0,
                model_calls: 0,
                stop_continuations: 0,
                doom_streak: 0,
                last_tool: None,
                bound_tools: HashMap::new(),
                call_ids: HashSet::new(),
                unresolved: VecDeque::new(),
                tool_completion: None,
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
    step: u32,
    model_calls: u32,
    stop_continuations: u32,
    /// Consecutive identical tool executions (name + arguments); drives
    /// `doom_loop_check` at `DOOM_LOOP_THRESHOLD`, mirroring OpenCode.
    doom_streak: u32,
    last_tool: Option<(String, String)>,
    unresolved: VecDeque<ToolCall>,
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
                    self.add_context(&[MAX_STEPS_PROMPT.into()]).await?;
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
                                let base = self
                                    .config
                                    .retry_delay
                                    .saturating_mul(1u32 << retries.min(10));
                                // Jitter avoids synchronized retries; zero stays useful for deterministic tests.
                                let jitter = if base.is_zero() {
                                    Duration::ZERO
                                } else {
                                    base / 4 * (uuid::Uuid::new_v4().as_u128() % 100) as u32 / 100
                                };
                                let delay = base
                                    .saturating_add(jitter)
                                    .min(self.config.retry_max_delay)
                                    .max(self.model.retry_after(&error).unwrap_or_default());
                                retries += 1;
                                self.send(Out::Retry {
                                    attempt: retries,
                                    max: self.config.max_model_retries,
                                    reason: model::retry_cause(&error),
                                    wait_ms: delay.as_millis().min(u64::MAX as u128) as u64,
                                })?;
                                // checked_add: an absurd server retry-after must not panic here.
                                // No operation timeout (OpenCode default) means the sleep bounds itself.
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
    ///
    /// OpenCode leaves an agent without `steps` unbounded at the outer loop but
    /// still caps each `streamText` run at 1000 (`stopWhen`). YourAI has a single
    /// step counter, so an unconfigured cap gets the `MAX_AGENT_STEPS` (1000)
    /// safety net; an explicit `steps` (tightened by per-turn `TurnLimits`) is
    /// honoured as-is, matching OpenCode's `agent.steps ?? Infinity` semantics.
    fn effective_steps(&self) -> Option<u32> {
        match (self.config.steps, self.tc.info.options.limits.steps) {
            (Some(configured), Some(limit)) => Some(configured.min(limit)),
            (Some(value), None) | (None, Some(value)) => Some(value),
            (None, None) => Some(MAX_AGENT_STEPS),
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
