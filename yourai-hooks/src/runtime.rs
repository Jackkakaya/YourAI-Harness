//! ConcreteHookRuntime：HookRuntime trait 的实现。
//!
//! 注册表 + 匹配 + 并行执行 + 输出解析 + 聚合。
//!
//! dispatch 流程（与 Claude Code 兼容）：
//! 1. 从事件提取 match_query
//! 2. 按 event_name + matcher 过滤注册项
//! 3. 为每个注册项创建 handler，并行执行（带超时）
//! 4. 解析每个 handler 的原始输出（command exit code / HTTP status / JSON）
//! 5. 校验 hookSpecificOutput.hookEventName 与输入事件一致
//! 6. 按 deny > ask > allow 聚合权限，收集 additional_contexts
//! 7. 返回 HookDispatchResult

use crate::config::{HandlerConfig, HookRegistration, HookSource, HooksConfig};
use crate::handler::{
    execute_with_timeout, handler_from_config_with_http_policy, HookModelExecutor,
};
use crate::wire_output::{
    ElicitationAction, HookJsonOutput, HookSpecificOutput, LegacyDecision, PermissionBehavior,
    SyncOutput,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tokio::sync::RwLock;
use tokio::task::JoinSet;
use yourai_core::hooks::{
    FailurePolicy, HookBlockingError, HookCommonOutcome, HookDispatchResult, HookHandler,
    HookInvocation, HookMessage, HookMessageKind, HookOutput, HookPermission, HookPointOutcome,
    HookRegistry, HookRun, HookRunStatus, HookRuntime, NativeHookRegistration,
    PermissionRequestBehavior, PermissionRequestDecision,
};
use yourai_core::{ErrorKind, YourAiError};

/// 单个 handler 对事件的贡献（解析后、聚合前）。
#[derive(Debug, Clone, Default)]
struct Contribution {
    hook_id: String,
    prevent_continuation: bool,
    stop_reason: Option<String>,
    system_message: Option<String>,
    suppress_output: bool,
    message: Option<(HookMessageKind, String)>,
    blocking_error: Option<String>,
    permission: Option<HookPermission>,
    updated_input: Option<Value>,
    additional_context: Option<String>,
    initial_user_message: Option<String>,
    watch_paths: Vec<String>,
    updated_mcp_tool_output: Option<Value>,
    retry: Option<bool>,
    permission_request_decision: Option<PermissionRequestDecision>,
    elicitation_action: Option<ElicitationAction>,
    elicitation_content: Option<Value>,
    worktree_path: Option<String>,
}

#[derive(Clone)]
enum RegisteredHandler {
    Config(HandlerConfig),
    Native(Arc<dyn HookHandler>),
}

#[derive(Clone)]
struct RegisteredHook {
    id: String,
    event_name: String,
    matcher: crate::matcher::CompiledMatcher,
    handler: RegisteredHandler,
    timeout: Option<Duration>,
    source: HookSource,
    failure_policy: FailurePolicy,
    once: bool,
    if_condition: Option<String>,
    status_message: Option<String>,
}

impl From<HookRegistration> for RegisteredHook {
    fn from(value: HookRegistration) -> Self {
        let if_condition = value.handler.if_condition().map(ToOwned::to_owned);
        let status_message = value.handler.status_message().map(ToOwned::to_owned);
        Self {
            id: value.id,
            event_name: value.event_name,
            matcher: value.matcher,
            handler: RegisteredHandler::Config(value.handler),
            timeout: value.timeout,
            source: value.source,
            failure_policy: value.failure_policy,
            once: value.once,
            if_condition,
            status_message,
        }
    }
}

// ── ConcreteHookRuntime ─────────────────────────────────────────────

/// HookRuntime 的具体实现：线程安全的注册表 + 分发。
pub struct ConcreteHookRuntime {
    registrations: RwLock<Vec<RegisteredHook>>,
    http_policy: crate::http::HttpHookPolicy,
    model_executor: Option<Arc<dyn HookModelExecutor>>,
    background_tx: tokio::sync::broadcast::Sender<crate::command::BackgroundHookEvent>,
}

impl ConcreteHookRuntime {
    pub fn new() -> Self {
        let (background_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            registrations: RwLock::new(Vec::new()),
            http_policy: crate::http::HttpHookPolicy::default(),
            model_executor: None,
            background_tx,
        }
    }

    pub fn with_http_policy(http_policy: crate::http::HttpHookPolicy) -> Self {
        let (background_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            registrations: RwLock::new(Vec::new()),
            http_policy,
            model_executor: None,
            background_tx,
        }
    }

    /// 注入 Prompt/Agent Hook 所需的模型能力。
    pub fn with_model_executor(mut self, executor: Arc<dyn HookModelExecutor>) -> Self {
        self.model_executor = Some(executor);
        self
    }

    /// 订阅后台 Hook 完成事件。未来 Loop 用它实现 `asyncRewake`。
    pub fn subscribe_background_events(
        &self,
    ) -> tokio::sync::broadcast::Receiver<crate::command::BackgroundHookEvent> {
        self.background_tx.subscribe()
    }

    /// 从配置批量注册。
    pub async fn register_config(
        &self,
        config: &HooksConfig,
        source: HookSource,
    ) -> Result<(), YourAiError> {
        for (event_name, groups) in &config.hooks {
            if yourai_core::hooks::HookEventKind::from_name(event_name).is_none() {
                return Err(ErrorKind::Config(format!("unknown hook event: {event_name}")).into());
            }
            for group in groups {
                for handler in &group.hooks {
                    handler.validate().map_err(ErrorKind::Config)?;
                    if handler.requires_model_executor() && self.model_executor.is_none() {
                        return Err(ErrorKind::Config(format!(
                            "{} hook requires a HookModelExecutor",
                            match handler {
                                HandlerConfig::Prompt { .. } => "prompt",
                                HandlerConfig::Agent { .. } => "agent",
                                _ => unreachable!(),
                            }
                        ))
                        .into());
                    }
                }
            }
        }
        let regs = crate::config::build_registrations(config, source);
        let mut guard = self.registrations.write().await;
        guard.extend(regs.into_iter().map(RegisteredHook::from));
        Ok(())
    }

    /// 注册单个注册项。
    pub async fn register(&self, reg: HookRegistration) {
        let mut guard = self.registrations.write().await;
        guard.push(reg.into());
    }

    /// 按 ID 注销。
    pub async fn unregister(&self, id: &str) {
        let mut guard = self.registrations.write().await;
        guard.retain(|r| r.id != id);
    }

    /// 分发一次 Hook 调用。各 handler 使用自己的配置超时。
    async fn dispatch_inner(
        &self,
        invocation: &HookInvocation,
    ) -> Result<HookDispatchResult, YourAiError> {
        let event = invocation.event_kind();
        let event_name = event.as_str();
        let match_query = invocation.event.match_query();

        let matched: Vec<RegisteredHook> = {
            let guard = self.registrations.read().await;
            let mut seen_config_handlers: Vec<(HookSource, HandlerConfig)> = Vec::new();
            guard
                .iter()
                .filter(|reg| {
                    reg.event_name == event_name
                        && handler_supported_for_event(&reg.handler, event_name)
                        && condition_matches(reg.if_condition.as_deref(), invocation)
                        && match match_query {
                            Some(q) => reg.matcher.matches(q),
                            None => true,
                        }
                })
                .filter(|reg| match &reg.handler {
                    RegisteredHandler::Config(config) => {
                        if seen_config_handlers
                            .iter()
                            .any(|(source, seen)| source == &reg.source && seen == config)
                        {
                            false
                        } else {
                            seen_config_handlers.push((reg.source.clone(), config.clone()));
                            true
                        }
                    }
                    RegisteredHandler::Native(_) => true,
                })
                .cloned()
                .collect()
        };

        if matched.is_empty() {
            return Ok(HookDispatchResult::empty(event));
        }

        let results = run_handlers_parallel(
            &matched,
            invocation,
            &self.http_policy,
            self.model_executor.as_ref(),
            &self.background_tx,
        )
        .await;

        let mut contributions = Vec::new();
        let mut runs = Vec::new();

        let mut once_ids = Vec::new();
        for executed in results {
            let reg = &matched[executed.registration_index];
            let result = executed.result;

            let mut run = HookRun {
                hook_id: reg.id.clone(),
                source: reg.source.as_str().to_string(),
                status: HookRunStatus::Completed,
                started_at: executed.started_at,
                duration: Duration::ZERO,
                stdout: None,
                stderr: None,
                exit_code: None,
                suppress_output: false,
                status_message: reg.status_message.clone(),
                background_task_id: None,
            };

            match result {
                HandlerExecResult::Success { output, duration } => {
                    run.duration = duration;
                    match parse_handler_output(&output, event_name) {
                        Ok(mut contrib) => {
                            contrib.hook_id = reg.id.clone();
                            run.suppress_output = contrib.suppress_output;
                            if let Some((HookMessageKind::NonBlockingError, message)) =
                                &contrib.message
                            {
                                run.status = HookRunStatus::Failed;
                                if reg.failure_policy == FailurePolicy::Closed {
                                    contrib = failure_contribution(reg, message.clone());
                                }
                            }
                            if reg.failure_policy == FailurePolicy::Closed {
                                if let HookOutput::Command {
                                    stderr, exit_code, ..
                                } = &output
                                {
                                    if *exit_code != 0 && *exit_code != 2 {
                                        contrib = failure_contribution(
                                            reg,
                                            format!("command failed (exit {exit_code}): {stderr}"),
                                        );
                                    }
                                }
                            }
                            match &output {
                                HookOutput::Command {
                                    stdout,
                                    stderr,
                                    exit_code,
                                } => {
                                    run.stdout = Some(stdout.clone());
                                    run.stderr = Some(stderr.clone());
                                    run.exit_code = Some(*exit_code);
                                    if *exit_code == 2 {
                                        run.status = HookRunStatus::Blocked;
                                    } else if *exit_code != 0 {
                                        run.status = HookRunStatus::Failed;
                                    }
                                }
                                HookOutput::Http { body, .. } => {
                                    run.stdout = Some(body.clone());
                                }
                                HookOutput::Backgrounded { task_id } => {
                                    run.status = HookRunStatus::Backgrounded;
                                    run.background_task_id = Some(task_id.clone());
                                }
                                HookOutput::Parsed(_) => {}
                                _ => {}
                            }
                            contributions.push(contrib);
                            runs.push(run);
                        }
                        Err(e) => {
                            run.status = HookRunStatus::Failed;
                            run.stderr = Some(e.to_string());
                            contributions.push(failure_contribution(reg, e.to_string()));
                            runs.push(run);
                        }
                    }
                }
                HandlerExecResult::Failed { error, duration } => {
                    run.duration = duration;
                    run.status = HookRunStatus::Failed;
                    run.stderr = Some(error.clone());
                    contributions.push(failure_contribution(reg, error));
                    runs.push(run);
                }
                HandlerExecResult::TimedOut { duration } => {
                    run.duration = duration;
                    run.status = HookRunStatus::TimedOut;
                    contributions.push(failure_contribution(
                        reg,
                        format!("hook timed out after {duration:?}"),
                    ));
                    runs.push(run);
                }
            }
            if reg.once {
                once_ids.push(reg.id.clone());
            }
        }

        if !once_ids.is_empty() {
            let mut guard = self.registrations.write().await;
            guard.retain(|reg| !once_ids.contains(&reg.id));
        }

        let common = aggregate_common(&contributions);
        let outcome = aggregate(event_name, contributions);
        Ok(HookDispatchResult {
            event,
            common,
            outcome,
            runs,
        })
    }
}

impl Default for ConcreteHookRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRuntime for ConcreteHookRuntime {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> yourai_core::BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(self.dispatch_inner(invocation))
    }
}

impl HookRegistry for ConcreteHookRuntime {
    fn register<'a>(
        &'a self,
        registration: NativeHookRegistration,
    ) -> yourai_core::BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let reg = RegisteredHook {
                id: registration.id,
                event_name: registration.event.as_str().to_string(),
                matcher: registration
                    .matcher
                    .as_deref()
                    .map(crate::matcher::CompiledMatcher::compile)
                    .unwrap_or(crate::matcher::CompiledMatcher::All),
                handler: RegisteredHandler::Native(registration.handler),
                timeout: registration.timeout,
                source: registration.source,
                failure_policy: registration.failure_policy,
                once: registration.once,
                if_condition: None,
                status_message: None,
            };
            self.registrations.write().await.push(reg);
            Ok(())
        })
    }

    fn unregister<'a>(
        &'a self,
        id: &'a str,
    ) -> yourai_core::BoxFuture<'a, Result<bool, YourAiError>> {
        Box::pin(async move {
            let mut guard = self.registrations.write().await;
            let before = guard.len();
            guard.retain(|reg| reg.id != id);
            Ok(before != guard.len())
        })
    }

    fn handler_ids<'a>(&'a self) -> yourai_core::BoxFuture<'a, Vec<String>> {
        Box::pin(async move {
            self.registrations
                .read()
                .await
                .iter()
                .map(|reg| reg.id.clone())
                .collect()
        })
    }
}

fn failure_contribution(reg: &RegisteredHook, message: String) -> Contribution {
    let mut contribution = Contribution {
        hook_id: reg.id.clone(),
        ..Contribution::default()
    };
    match reg.failure_policy {
        FailurePolicy::Open => {
            contribution.message = Some((HookMessageKind::NonBlockingError, message));
        }
        FailurePolicy::Closed => {
            contribution.prevent_continuation = true;
            contribution.stop_reason = Some(message.clone());
            contribution.blocking_error = Some(message);
        }
        _ => {}
    }
    contribution
}

fn handler_supported_for_event(handler: &RegisteredHandler, event_name: &str) -> bool {
    !matches!(
        (handler, event_name),
        (
            RegisteredHandler::Config(HandlerConfig::Http { .. }),
            "SessionStart" | "Setup"
        )
    )
}

/// Claude 的 `if` 使用 permission-rule 形式。这里覆盖通用工具字段；未来工具可通过
/// Loop 侧的专用 permission matcher 提供更精确的语义。
fn condition_matches(condition: Option<&str>, invocation: &HookInvocation) -> bool {
    let Some(condition) = condition else {
        return true;
    };
    let (tool_name, tool_input) = match &invocation.event {
        yourai_core::hooks::HookEvent::PreToolUse {
            tool_name,
            tool_input,
            ..
        }
        | yourai_core::hooks::HookEvent::PostToolUse {
            tool_name,
            tool_input,
            ..
        }
        | yourai_core::hooks::HookEvent::PostToolUseFailure {
            tool_name,
            tool_input,
            ..
        }
        | yourai_core::hooks::HookEvent::PermissionRequest {
            tool_name,
            tool_input,
            ..
        } => (tool_name.as_str(), tool_input),
        _ => return false,
    };

    let condition = condition.trim();
    let Some(open) = condition.find('(') else {
        return crate::matcher::normalize_legacy_tool_name(condition)
            == crate::matcher::normalize_legacy_tool_name(tool_name);
    };
    if !condition.ends_with(')')
        || crate::matcher::normalize_legacy_tool_name(condition[..open].trim())
            != crate::matcher::normalize_legacy_tool_name(tool_name)
    {
        return false;
    }
    let pattern = &condition[open + 1..condition.len() - 1];
    if pattern.is_empty() {
        return true;
    }

    let Some(object) = tool_input.as_object() else {
        return false;
    };
    ["command", "file_path", "path", "pattern", "query"]
        .iter()
        .filter_map(|key| object.get(*key).and_then(Value::as_str))
        .any(|candidate| wildcard_matches(pattern, candidate))
}

fn wildcard_matches(pattern: &str, candidate: &str) -> bool {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        if ch == '*' {
            regex.push_str(".*");
        } else {
            regex.push_str(&regex::escape(&ch.to_string()));
        }
    }
    regex.push('$');
    regex::Regex::new(&regex)
        .map(|compiled| compiled.is_match(candidate))
        .unwrap_or(false)
}

// ── Parallel execution ──────────────────────────────────────────────

enum HandlerExecResult {
    Success {
        output: HookOutput,
        duration: Duration,
    },
    Failed {
        #[allow(dead_code)]
        error: String,
        duration: Duration,
    },
    TimedOut {
        duration: Duration,
    },
}

struct ExecutedHook {
    registration_index: usize,
    started_at: i64,
    result: HandlerExecResult,
}

async fn run_handlers_parallel(
    matched: &[RegisteredHook],
    invocation: &HookInvocation,
    http_policy: &crate::http::HttpHookPolicy,
    model_executor: Option<&Arc<dyn HookModelExecutor>>,
    background_tx: &tokio::sync::broadcast::Sender<crate::command::BackgroundHookEvent>,
) -> Vec<ExecutedHook> {
    let mut join_set: JoinSet<ExecutedHook> = JoinSet::new();

    for (registration_index, reg) in matched.iter().enumerate() {
        let inv = invocation.clone();
        let timeout = reg.timeout;
        let http_policy = http_policy.clone();
        let model_executor = model_executor.cloned();
        let background_tx = background_tx.clone();
        let handler: Arc<dyn HookHandler> = match &reg.handler {
            RegisteredHandler::Config(config) => match config {
                HandlerConfig::Command { command, shell, .. } => Arc::new(
                    crate::command::CommandHandler::new(command.clone(), *shell).with_background(
                        crate::command::BackgroundCommandContext {
                            hook_id: reg.id.clone(),
                            event_name: reg.event_name.clone(),
                            rewake: config.async_rewake(),
                            timeout: reg.timeout,
                            force_background: config.is_async(),
                            sender: background_tx,
                        },
                    ),
                ),
                _ => Arc::from(handler_from_config_with_http_policy(
                    config,
                    http_policy,
                    model_executor,
                )),
            },
            RegisteredHandler::Native(handler) => handler.clone(),
        };

        join_set.spawn(async move {
            let start = std::time::Instant::now();
            let started_at = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis() as i64;
            let exec_future = execute_with_timeout(handler.as_ref(), &inv, timeout);
            let result = match exec_future.await {
                Ok(output) => HandlerExecResult::Success {
                    output,
                    duration: start.elapsed(),
                },
                Err(error) => {
                    let message = error.to_string();
                    if message.contains("timed out") {
                        HandlerExecResult::TimedOut {
                            duration: start.elapsed(),
                        }
                    } else {
                        HandlerExecResult::Failed {
                            error: message,
                            duration: start.elapsed(),
                        }
                    }
                }
            };
            ExecutedHook {
                registration_index,
                started_at,
                result,
            }
        });
    }

    let mut results = Vec::with_capacity(matched.len());
    while let Some(res) = join_set.join_next().await {
        if let Ok(executed) = res {
            results.push(executed);
        }
    }
    results
}

// ── Output parsing ──────────────────────────────────────────────────

fn parse_handler_output(
    output: &HookOutput,
    expected_event: &str,
) -> Result<Contribution, YourAiError> {
    let mut contrib = Contribution::default();

    match output {
        HookOutput::Parsed(json_value) => {
            let json_output: HookJsonOutput = parse_json_value(json_value)?;
            apply_json_output(&json_output, expected_event, &mut contrib)?;
        }
        HookOutput::Command {
            stdout,
            stderr,
            exit_code,
        } => match exit_code {
            0 => {
                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    // 空输出 = 空成功
                } else if trimmed.starts_with('{') {
                    match crate::wire_output::parse_hook_json(stdout) {
                        Ok(json_output) => {
                            apply_json_output(&json_output, expected_event, &mut contrib)?;
                        }
                        Err(e) => {
                            let kind = match e.classify() {
                                serde_json::error::Category::Syntax
                                | serde_json::error::Category::Eof => HookMessageKind::Success,
                                serde_json::error::Category::Data
                                | serde_json::error::Category::Io => {
                                    HookMessageKind::NonBlockingError
                                }
                            };
                            let content = if kind == HookMessageKind::Success {
                                trimmed.to_string()
                            } else {
                                format!("Hook JSON output validation failed: {e}")
                            };
                            contrib.message = Some((kind, content));
                        }
                    }
                } else {
                    contrib.message = Some((HookMessageKind::Success, trimmed.to_string()));
                }
            }
            2 => {
                contrib.blocking_error = Some(if stderr.trim().is_empty() {
                    "blocked by hook (exit 2)".to_string()
                } else {
                    stderr.trim().to_string()
                });
            }
            _ => {
                contrib.message = Some((
                    HookMessageKind::NonBlockingError,
                    format!(
                        "Hook failed with non-blocking status {exit_code}: {}",
                        if stderr.trim().is_empty() {
                            "No stderr output"
                        } else {
                            stderr.trim()
                        }
                    ),
                ));
            }
        },
        HookOutput::Http { body, status: _ } => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                // 空 body = 空成功
            } else if trimmed.starts_with('{') {
                match crate::wire_output::parse_hook_json(body) {
                    Ok(json_output) => {
                        apply_json_output(&json_output, expected_event, &mut contrib)?;
                    }
                    Err(e) => {
                        contrib.message = Some((
                            HookMessageKind::NonBlockingError,
                            format!("HTTP Hook JSON output validation failed: {e}"),
                        ));
                    }
                }
            } else {
                contrib.message = Some((
                    HookMessageKind::NonBlockingError,
                    format!("HTTP Hook must return JSON, got: {trimmed}"),
                ));
            }
        }
        _ => {}
    }

    Ok(contrib)
}

fn parse_json_value(value: &Value) -> Result<HookJsonOutput, YourAiError> {
    if value.get("async") == Some(&Value::Bool(true)) {
        let async_output = serde_json::from_value::<crate::wire_output::AsyncOutput>(value.clone())
            .map_err(|e| ErrorKind::Provider {
                name: "hook",
                message: format!("async output parse: {e}"),
            })?;
        Ok(HookJsonOutput::Async(async_output))
    } else {
        let sync_output = serde_json::from_value::<SyncOutput>(value.clone()).map_err(|e| {
            ErrorKind::Provider {
                name: "hook",
                message: format!("sync output parse: {e}"),
            }
        })?;
        Ok(HookJsonOutput::Sync(sync_output))
    }
}

fn apply_json_output(
    json_output: &HookJsonOutput,
    expected_event: &str,
    contrib: &mut Contribution,
) -> Result<(), YourAiError> {
    match json_output {
        HookJsonOutput::Async(_) => {}
        HookJsonOutput::Sync(s) => {
            apply_sync_output(s, expected_event, contrib)?;
        }
    }
    Ok(())
}

fn apply_sync_output(
    s: &SyncOutput,
    expected_event: &str,
    contrib: &mut Contribution,
) -> Result<(), YourAiError> {
    contrib.suppress_output = s.suppress_output.unwrap_or(false);

    if s.continue_field == Some(false) {
        contrib.prevent_continuation = true;
        contrib.stop_reason = s.stop_reason.clone();
    }

    if let Some(msg) = &s.system_message {
        contrib.system_message = Some(msg.clone());
    }

    if let Some(decision) = &s.decision {
        match decision {
            LegacyDecision::Approve => {
                contrib.permission = Some(HookPermission::Allow {
                    reason: s.reason.clone(),
                });
            }
            LegacyDecision::Block => {
                let reason = s
                    .reason
                    .clone()
                    .unwrap_or_else(|| "blocked by hook".to_string());
                contrib.permission = Some(HookPermission::Deny {
                    reason: reason.clone(),
                });
                contrib.blocking_error = Some(reason);
            }
        }
    }

    if let Some(hso) = &s.hook_specific_output {
        let hso_event = hso.event_name();
        if hso_event != expected_event {
            return Err(ErrorKind::Provider {
                name: "hook",
                message: format!(
                    "hookEventName mismatch: expected {expected_event}, got {hso_event}"
                ),
            }
            .into());
        }
        apply_hook_specific_output(hso, contrib);
    }

    Ok(())
}

fn apply_hook_specific_output(hso: &HookSpecificOutput, contrib: &mut Contribution) {
    match hso {
        HookSpecificOutput::PreToolUse {
            permission_decision,
            permission_decision_reason,
            updated_input,
            additional_context,
        } => {
            if let Some(pd) = permission_decision {
                match pd {
                    PermissionBehavior::Allow => {
                        contrib.permission = Some(HookPermission::Allow {
                            reason: permission_decision_reason.clone(),
                        });
                    }
                    PermissionBehavior::Deny => {
                        let reason = permission_decision_reason
                            .clone()
                            .unwrap_or_else(|| "denied by hook".to_string());
                        contrib.permission = Some(HookPermission::Deny {
                            reason: reason.clone(),
                        });
                        contrib.blocking_error = Some(reason);
                    }
                    PermissionBehavior::Ask => {
                        contrib.permission = Some(HookPermission::Ask {
                            reason: permission_decision_reason
                                .clone()
                                .unwrap_or_else(|| "hook requests user confirmation".to_string()),
                        });
                    }
                }
            }
            if let Some(ui) = updated_input {
                contrib.updated_input = Some(Value::Object(ui.clone()));
            }
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
        }
        HookSpecificOutput::UserPromptSubmit { additional_context }
        | HookSpecificOutput::Setup { additional_context }
        | HookSpecificOutput::SubagentStart { additional_context }
        | HookSpecificOutput::PostToolUseFailure { additional_context }
        | HookSpecificOutput::Notification { additional_context } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
        }
        HookSpecificOutput::SessionStart {
            additional_context,
            initial_user_message,
            watch_paths,
        } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
            if let Some(msg) = initial_user_message {
                contrib.initial_user_message = Some(msg.clone());
            }
            if let Some(wp) = watch_paths {
                contrib.watch_paths = wp.clone();
            }
        }
        HookSpecificOutput::PostToolUse {
            additional_context,
            updated_mcp_tool_output,
        } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
            if let Some(umto) = updated_mcp_tool_output {
                contrib.updated_mcp_tool_output = Some(umto.clone());
            }
        }
        HookSpecificOutput::PermissionDenied { retry } => {
            contrib.retry = *retry;
        }
        HookSpecificOutput::PermissionRequest { decision } => {
            let (behavior, updated_input, updated_permissions, message, interrupt) = match decision
            {
                crate::wire_output::PermissionRequestDecision::Allow {
                    updated_input,
                    updated_permissions,
                } => (
                    PermissionRequestBehavior::Allow,
                    updated_input.clone().map(Value::Object),
                    updated_permissions
                        .as_ref()
                        .map(|v| {
                            v.iter()
                                .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
                                .collect()
                        })
                        .unwrap_or_default(),
                    None,
                    false,
                ),
                crate::wire_output::PermissionRequestDecision::Deny { message, interrupt } => (
                    PermissionRequestBehavior::Deny,
                    None,
                    vec![],
                    message.clone(),
                    interrupt.unwrap_or(false),
                ),
            };
            contrib.permission_request_decision = Some(PermissionRequestDecision {
                behavior,
                updated_input,
                updated_permissions,
                message,
                interrupt,
            });
        }
        HookSpecificOutput::CwdChanged { watch_paths }
        | HookSpecificOutput::FileChanged { watch_paths } => {
            if let Some(wp) = watch_paths {
                contrib.watch_paths = wp.clone();
            }
        }
        HookSpecificOutput::Elicitation { action, content }
        | HookSpecificOutput::ElicitationResult { action, content } => {
            contrib.elicitation_action = *action;
            contrib.elicitation_content = content.clone().map(Value::Object);
            if *action == Some(ElicitationAction::Decline) {
                contrib.blocking_error = Some("elicitation declined by hook".to_string());
            }
        }
        HookSpecificOutput::WorktreeCreate { worktree_path } => {
            contrib.worktree_path = Some(worktree_path.clone());
        }
    }
}

// ── Aggregation ─────────────────────────────────────────────────────

fn aggregate(event_name: &str, contributions: Vec<Contribution>) -> HookPointOutcome {
    match event_name {
        "PreToolUse" => HookPointOutcome::PreToolUse(aggregate_pre_tool_use(contributions)),
        "PostToolUse" => HookPointOutcome::PostToolUse(aggregate_post_tool_use(contributions)),
        "UserPromptSubmit" => {
            HookPointOutcome::UserPromptSubmit(aggregate_user_prompt_submit(contributions))
        }
        "SessionStart" => HookPointOutcome::SessionStart(aggregate_session_start(contributions)),
        "PermissionDenied" => {
            HookPointOutcome::PermissionDenied(aggregate_permission_denied(contributions))
        }
        "PermissionRequest" => {
            HookPointOutcome::PermissionRequest(aggregate_permission_request(contributions))
        }
        "CwdChanged" | "FileChanged" => {
            let wp = aggregate_watch_paths(contributions);
            if event_name == "CwdChanged" {
                HookPointOutcome::CwdChanged(wp)
            } else {
                HookPointOutcome::FileChanged(wp)
            }
        }
        "Elicitation" => HookPointOutcome::Elicitation(aggregate_elicitation(contributions)),
        "ElicitationResult" => {
            HookPointOutcome::ElicitationResult(aggregate_elicitation(contributions))
        }
        "WorktreeCreate" => {
            HookPointOutcome::WorktreeCreate(aggregate_worktree_create(contributions))
        }
        _ => HookPointOutcome::Generic(aggregate_generic(contributions)),
    }
}

fn aggregate_common(contributions: &[Contribution]) -> HookCommonOutcome {
    let prevent_continuation = contributions.iter().any(|c| c.prevent_continuation);
    let stop_reason = contributions
        .iter()
        .rev()
        .find_map(|c| c.stop_reason.clone());
    let system_messages = contributions
        .iter()
        .filter_map(|c| c.system_message.clone())
        .collect();
    let messages = contributions
        .iter()
        .filter_map(|c| {
            c.message.clone().map(|(kind, content)| HookMessage {
                hook_id: c.hook_id.clone(),
                kind,
                content,
            })
        })
        .collect();
    let blocking_errors = contributions
        .iter()
        .filter_map(|c| {
            c.blocking_error.clone().map(|message| HookBlockingError {
                hook_id: c.hook_id.clone(),
                message,
            })
        })
        .collect();

    HookCommonOutcome {
        prevent_continuation,
        stop_reason,
        system_messages,
        messages,
        blocking_errors,
    }
}

fn aggregate_contexts(contributions: &[Contribution]) -> Vec<String> {
    contributions
        .iter()
        .filter_map(|c| c.additional_context.clone())
        .collect()
}

fn aggregate_pre_tool_use(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PreToolUseOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let mut permission = HookPermission::Pass;
    let mut updated_input = None;

    for c in &contributions {
        if let Some(p) = &c.permission {
            permission = permission.merge(p);
        }
        if let Some(ui) = &c.updated_input {
            if !matches!(&c.permission, Some(HookPermission::Deny { .. })) {
                updated_input = Some(ui.clone());
            }
        }
    }

    yourai_core::hooks::PreToolUseOutcome {
        permission,
        updated_input,
        additional_contexts,
    }
}

fn aggregate_post_tool_use(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PostToolUseOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let updated_mcp_tool_output = contributions
        .iter()
        .rev()
        .find_map(|c| c.updated_mcp_tool_output.clone());

    yourai_core::hooks::PostToolUseOutcome {
        additional_contexts,
        updated_mcp_tool_output,
    }
}

fn aggregate_user_prompt_submit(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::UserPromptSubmitOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    yourai_core::hooks::UserPromptSubmitOutcome {
        additional_contexts,
    }
}

fn aggregate_session_start(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::SessionStartOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let initial_user_message = contributions
        .iter()
        .rev()
        .find_map(|c| c.initial_user_message.clone());
    let watch_paths: Vec<String> = contributions
        .iter()
        .flat_map(|c| c.watch_paths.clone())
        .collect();

    yourai_core::hooks::SessionStartOutcome {
        additional_contexts,
        initial_user_message,
        watch_paths,
    }
}

fn aggregate_permission_denied(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PermissionDeniedOutcome {
    let retry = contributions.iter().any(|c| c.retry == Some(true));
    yourai_core::hooks::PermissionDeniedOutcome { retry }
}

fn aggregate_permission_request(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PermissionRequestOutcome {
    let decision = contributions
        .iter()
        .rev()
        .find_map(|c| c.permission_request_decision.clone());
    yourai_core::hooks::PermissionRequestOutcome { decision }
}

fn aggregate_watch_paths(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::WatchPathsOutcome {
    let watch_paths: Vec<String> = contributions
        .iter()
        .flat_map(|c| c.watch_paths.clone())
        .collect();
    yourai_core::hooks::WatchPathsOutcome { watch_paths }
}

fn aggregate_elicitation(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::ElicitationOutcome {
    let action = contributions
        .iter()
        .rev()
        .find_map(|c| c.elicitation_action);
    let content = contributions
        .iter()
        .rev()
        .find_map(|c| c.elicitation_content.clone());
    yourai_core::hooks::ElicitationOutcome {
        action: action.map(|value| {
            match value {
                ElicitationAction::Accept => "accept",
                ElicitationAction::Decline => "decline",
                ElicitationAction::Cancel => "cancel",
            }
            .to_string()
        }),
        content,
    }
}

fn aggregate_worktree_create(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::WorktreeCreateOutcome {
    let worktree_path = contributions
        .iter()
        .rev()
        .find_map(|c| c.worktree_path.clone());
    yourai_core::hooks::WorktreeCreateOutcome { worktree_path }
}

fn aggregate_generic(contributions: Vec<Contribution>) -> yourai_core::hooks::GenericOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    yourai_core::hooks::GenericOutcome {
        additional_contexts,
    }
}

/// 便捷构造：Arc<ConcreteHookRuntime>。
pub fn new_runtime() -> Arc<ConcreteHookRuntime> {
    Arc::new(ConcreteHookRuntime::new())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::handler::{BoxFuture, HookModelDecision, HookModelExecutor, HookModelRequest};
    use std::sync::Mutex;
    use yourai_core::hooks::{BaseInput, HookEvent};

    struct RecordingModelExecutor {
        requests: Mutex<Vec<HookModelRequest>>,
        decision: HookModelDecision,
    }

    impl HookModelExecutor for RecordingModelExecutor {
        fn evaluate<'a>(
            &'a self,
            request: HookModelRequest,
        ) -> BoxFuture<'a, Result<HookModelDecision, YourAiError>> {
            self.requests.lock().unwrap().push(request);
            let decision = self.decision.clone();
            Box::pin(async move { Ok(decision) })
        }
    }

    fn command_registration(
        id: &str,
        event_name: &str,
        matcher: crate::matcher::CompiledMatcher,
        command: &str,
    ) -> HookRegistration {
        HookRegistration {
            id: id.to_string(),
            event_name: event_name.to_string(),
            matcher,
            handler: crate::config::HandlerConfig::Command {
                command: command.to_string(),
                shell: None,
                timeout: Some(5.0),
                if_condition: None,
                once: None,
                is_async: None,
                async_rewake: None,
                status_message: None,
                failure_policy: None,
            },
            timeout: Some(Duration::from_secs(5)),
            source: HookSource::Session,
            failure_policy: FailurePolicy::Open,
            once: false,
        }
    }

    #[tokio::test]
    async fn dispatch_empty_when_no_handlers() {
        let rt = ConcreteHookRuntime::new();
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = rt.dispatch(&inv).await.unwrap();
        assert!(result.runs.is_empty());
        match result.outcome {
            HookPointOutcome::UserPromptSubmit(o) => {
                assert!(o.additional_contexts.is_empty());
            }
            _ => panic!("expected UserPromptSubmit outcome"),
        }
    }

    #[tokio::test]
    async fn prompt_and_agent_handlers_use_injected_model_executor() {
        let executor = Arc::new(RecordingModelExecutor {
            requests: Mutex::new(Vec::new()),
            decision: HookModelDecision {
                ok: true,
                reason: None,
            },
        });
        let rt = ConcreteHookRuntime::new().with_model_executor(executor.clone());
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [
                        {"type": "prompt", "prompt": "review $ARGUMENTS", "model": "fast"},
                        {"type": "agent", "prompt": "investigate $ARGUMENTS"}
                    ]
                }]
            }
        }))
        .unwrap();
        rt.register_config(&config, HookSource::Project)
            .await
            .unwrap();

        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hello".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(result.runs.len(), 2);
        assert!(!result.common.prevent_continuation);

        let requests = executor.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert!(requests
            .iter()
            .all(|request| request.prompt.contains("UserPromptSubmit")));
        assert!(requests.iter().any(|request| request.agentic));
        assert!(requests.iter().any(|request| !request.agentic));
        assert!(requests
            .iter()
            .any(|request| request.model.as_deref() == Some("fast")));
    }

    #[tokio::test]
    async fn prompt_handler_requires_model_executor_at_registration() {
        let rt = ConcreteHookRuntime::new();
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{"type": "prompt", "prompt": "review"}]
                }]
            }
        }))
        .unwrap();
        let error = rt
            .register_config(&config, HookSource::Project)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HookModelExecutor"));
    }

    #[tokio::test]
    async fn rejected_prompt_handler_blocks_with_reason() {
        let executor = Arc::new(RecordingModelExecutor {
            requests: Mutex::new(Vec::new()),
            decision: HookModelDecision {
                ok: false,
                reason: Some("model rejected prompt".to_string()),
            },
        });
        let rt = ConcreteHookRuntime::new().with_model_executor(executor);
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{
                    "hooks": [{"type": "prompt", "prompt": "review $ARGUMENTS"}]
                }]
            }
        }))
        .unwrap();
        rt.register_config(&config, HookSource::Project)
            .await
            .unwrap();
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hello".to_string(),
                },
            ))
            .await
            .unwrap();
        assert!(result.common.prevent_continuation);
        assert_eq!(
            result.common.stop_reason.as_deref(),
            Some("model rejected prompt")
        );
        assert_eq!(
            result.common.blocking_errors[0].message,
            "model rejected prompt"
        );
    }

    #[tokio::test]
    async fn dispatch_command_handler_echo_context() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "echo-1",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            r#"printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"hello from hook"}}'"#,
        ))
        .await;

        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = rt.dispatch(&inv).await.unwrap();
        assert_eq!(result.runs.len(), 1);
        match result.outcome {
            HookPointOutcome::UserPromptSubmit(o) => {
                assert_eq!(o.additional_contexts, vec!["hello from hook".to_string()]);
            }
            _ => panic!("expected UserPromptSubmit outcome"),
        }
    }

    #[tokio::test]
    async fn duplicate_config_handlers_from_same_source_run_once() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "duplicate-1",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf once",
        ))
        .await;
        rt.register(command_registration(
            "duplicate-2",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf once",
        ))
        .await;

        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(result.runs.len(), 1);
        assert_eq!(result.runs[0].hook_id, "duplicate-1");
    }

    #[tokio::test]
    async fn dispatch_command_handler_exit2_blocks() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "block-1",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "echo 'forbidden' >&2; exit 2",
        ))
        .await;

        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = rt.dispatch(&inv).await.unwrap();
        assert_eq!(result.common.blocking_errors.len(), 1);
        assert!(result.common.blocking_errors[0]
            .message
            .contains("forbidden"));
        assert!(!result.common.prevent_continuation);
    }

    #[tokio::test]
    async fn dispatch_pretooluse_deny_overrides_allow() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "allow-1",
            "PreToolUse",
            crate::matcher::CompiledMatcher::compile("Bash"),
            r#"printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}'"#,
        ))
        .await;
        rt.register(command_registration(
            "deny-1",
            "PreToolUse",
            crate::matcher::CompiledMatcher::compile("Bash"),
            r#"printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"no rm -rf"}}'"#,
        ))
        .await;

        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::PreToolUse {
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command": "rm -rf /"}),
                tool_use_id: "call_1".to_string(),
            },
        );
        let result = rt.dispatch(&inv).await.unwrap();
        assert_eq!(result.runs.len(), 2);
        assert!(!result.common.prevent_continuation);
        match result.outcome {
            HookPointOutcome::PreToolUse(o) => match o.permission {
                HookPermission::Deny { reason } => {
                    assert_eq!(reason, "no rm -rf");
                }
                _ => panic!("expected deny, got {:?}", o.permission),
            },
            _ => panic!("expected PreToolUse outcome"),
        }
    }

    #[tokio::test]
    async fn event_name_mismatch_errors() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "mismatch-1",
            "PreToolUse",
            crate::matcher::CompiledMatcher::compile("Bash"),
            r#"printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"wrong event"}}'"#,
        ))
        .await;

        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::PreToolUse {
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({}),
                tool_use_id: "call_1".to_string(),
            },
        );
        let result = rt.dispatch(&inv).await.unwrap();
        assert_eq!(result.runs.len(), 1);
        assert_eq!(result.runs[0].status, HookRunStatus::Failed);
    }

    #[tokio::test]
    async fn completion_order_preserves_registration_identity() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "slow",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "sleep 0.05; printf slow",
        ))
        .await;
        rt.register(command_registration(
            "fast",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf fast",
        ))
        .await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();

        assert_eq!(result.runs[0].hook_id, "fast");
        assert_eq!(result.runs[0].stdout.as_deref(), Some("fast"));
        assert_eq!(result.runs[1].hook_id, "slow");
        assert_eq!(result.runs[1].stdout.as_deref(), Some("slow"));
    }

    #[tokio::test]
    async fn plain_text_is_message_not_additional_context() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "plain",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf remembered",
        ))
        .await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert!(result.outcome.event_name() == "UserPromptSubmit");
        assert_eq!(result.common.messages.len(), 1);
        assert_eq!(result.common.messages[0].content, "remembered");
        match result.outcome {
            HookPointOutcome::UserPromptSubmit(outcome) => {
                assert!(outcome.additional_contexts.is_empty());
            }
            _ => panic!("expected UserPromptSubmit"),
        }
    }

    #[tokio::test]
    async fn common_fields_survive_typed_outcome() {
        let rt = ConcreteHookRuntime::new();
        rt.register(command_registration(
            "common",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            r#"printf '{"continue":false,"suppressOutput":true,"stopReason":"stop","systemMessage":"notice","hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"ctx"}}'"#,
        ))
        .await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert!(result.common.prevent_continuation);
        assert_eq!(result.common.stop_reason.as_deref(), Some("stop"));
        assert_eq!(result.common.system_messages, vec!["notice"]);
        assert!(result.runs[0].suppress_output);
    }

    #[tokio::test]
    async fn once_hook_is_removed_after_execution() {
        let rt = ConcreteHookRuntime::new();
        let mut registration = command_registration(
            "once",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf once",
        );
        registration.once = true;
        rt.register(registration).await;
        let invocation = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        assert_eq!(rt.dispatch(&invocation).await.unwrap().runs.len(), 1);
        assert!(rt.dispatch(&invocation).await.unwrap().runs.is_empty());
    }

    #[tokio::test]
    async fn if_condition_filters_before_spawn() {
        let rt = ConcreteHookRuntime::new();
        let mut registration = command_registration(
            "git-only",
            "PreToolUse",
            crate::matcher::CompiledMatcher::All,
            "printf should-not-run",
        );
        if let HandlerConfig::Command { if_condition, .. } = &mut registration.handler {
            *if_condition = Some("Bash(git *)".to_string());
        }
        rt.register(registration).await;
        let invocation = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::PreToolUse {
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command":"cargo test"}),
                tool_use_id: "call".to_string(),
            },
        );
        assert!(rt.dispatch(&invocation).await.unwrap().runs.is_empty());
    }

    #[tokio::test]
    async fn async_rewake_backgrounds_and_reports_completion() {
        let rt = ConcreteHookRuntime::new();
        let mut receiver = rt.subscribe_background_events();
        let mut registration = command_registration(
            "async",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "sleep 0.02; echo wake >&2; exit 2",
        );
        if let HandlerConfig::Command { async_rewake, .. } = &mut registration.handler {
            *async_rewake = Some(true);
        }
        rt.register(registration).await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::Backgrounded);
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.hook_id, "async");
        assert_eq!(event.exit_code, 2);
        assert!(event.rewake);
        assert!(event.stderr.contains("wake"));
    }

    #[tokio::test]
    async fn stdout_async_handshake_backgrounds_running_process() {
        let rt = ConcreteHookRuntime::new();
        let mut receiver = rt.subscribe_background_events();
        rt.register(command_registration(
            "handshake",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf '{\"async\":true}\\n'; sleep 0.02; printf done",
        ))
        .await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::Backgrounded);
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(event.stdout, "done");
        assert_eq!(event.exit_code, 0);
    }

    #[tokio::test]
    async fn stdout_async_timeout_starts_before_process_exit() {
        let rt = ConcreteHookRuntime::new();
        let mut receiver = rt.subscribe_background_events();
        rt.register(command_registration(
            "handshake-timeout",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "printf '{\"async\":true,\"asyncTimeout\":20}\\n'; sleep 5",
        ))
        .await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::Backgrounded);
        let event = tokio::time::timeout(Duration::from_secs(1), receiver.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(event.timed_out);
        assert_eq!(event.exit_code, -1);
    }

    #[tokio::test]
    async fn closed_failure_policy_prevents_continuation() {
        let rt = ConcreteHookRuntime::new();
        let mut registration = command_registration(
            "closed",
            "UserPromptSubmit",
            crate::matcher::CompiledMatcher::All,
            "exit 1",
        );
        registration.failure_policy = FailurePolicy::Closed;
        rt.register(registration).await;
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        assert!(result.common.prevent_continuation);
        assert_eq!(result.common.blocking_errors.len(), 1);
    }

    #[tokio::test]
    async fn native_registration_uses_core_registry_contract() {
        let rt = ConcreteHookRuntime::new();
        let handler = crate::handler::NativeHandler::new(|_| {
            Ok(HookJsonOutput::Sync(SyncOutput {
                hook_specific_output: Some(HookSpecificOutput::UserPromptSubmit {
                    additional_context: Some("native context".to_string()),
                }),
                ..SyncOutput::default()
            }))
        });
        HookRegistry::register(
            &rt,
            NativeHookRegistration {
                id: "native".to_string(),
                event: yourai_core::hooks::HookEventKind::UserPromptSubmit,
                matcher: None,
                handler: Arc::new(handler),
                timeout: None,
                source: HookSource::Session,
                failure_policy: FailurePolicy::Open,
                once: false,
            },
        )
        .await
        .unwrap();
        let result = rt
            .dispatch(&HookInvocation::new(
                BaseInput::new("sess", "/tmp"),
                HookEvent::UserPromptSubmit {
                    prompt: "hi".to_string(),
                },
            ))
            .await
            .unwrap();
        match result.outcome {
            HookPointOutcome::UserPromptSubmit(outcome) => {
                assert_eq!(outcome.additional_contexts, vec!["native context"]);
            }
            _ => panic!("expected UserPromptSubmit"),
        }
    }

    #[test]
    fn unit_aggregate_deny_ask_allow() {
        let contributions = vec![
            Contribution {
                permission: Some(HookPermission::Allow { reason: None }),
                ..Default::default()
            },
            Contribution {
                permission: Some(HookPermission::Ask {
                    reason: "please confirm".to_string(),
                }),
                ..Default::default()
            },
        ];
        let outcome = aggregate_pre_tool_use(contributions);
        assert!(matches!(outcome.permission, HookPermission::Ask { .. }));

        let contributions = vec![
            Contribution {
                permission: Some(HookPermission::Ask {
                    reason: "please confirm".to_string(),
                }),
                ..Default::default()
            },
            Contribution {
                permission: Some(HookPermission::Deny {
                    reason: "no way".to_string(),
                }),
                ..Default::default()
            },
        ];
        let outcome = aggregate_pre_tool_use(contributions);
        assert!(matches!(outcome.permission, HookPermission::Deny { .. }));
    }
}
