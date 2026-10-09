//! DefaultHookRuntime：HookRuntime trait 的实现。
//!
//! 注册表 + 匹配 + 并行执行 + 输出解析 + 聚合。
//!
//! dispatch 流程（与 Claude Code 兼容）：
//! 1. 从事件提取 match_query
//! 2. 按 event_name + matcher 过滤注册项
//! 3. 锁内认领 once 注册项，并行执行已编译 handler（带超时）
//! 4. 解析每个 handler 的原始输出（command exit code / HTTP status / JSON）
//! 5. 校验 hookSpecificOutput.hookEventName 与输入事件一致
//! 6. 按 deny > ask > allow 聚合权限，收集 additional_contexts
//! 7. 返回 HookDispatchResult

mod outcomes;
use outcomes::*;

use crate::hooks::config::{HookRegistration, HookSource, HooksConfig};
use crate::hooks::handler::{execute_with_timeout, handler_from_config, HookEvaluator};
use crate::hooks::wire_output::{
    ElicitationAction, HookJsonOutput, HookSpecificOutput, LegacyDecision, PermissionBehavior,
    SyncOutput,
};
use serde_json::Value;
use std::collections::HashMap;
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
struct RegisteredHook {
    id: String,
    event: yourai_core::hooks::HookEventKind,
    matcher: crate::hooks::matcher::CompiledMatcher,
    handler: Arc<dyn HookHandler>,
    config_identity: Option<String>,
    timeout: Option<Duration>,
    source: HookSource,
    failure_policy: FailurePolicy,
    once: bool,
    if_condition: Option<String>,
    status_message: Option<String>,
}

// ── DefaultHookRuntime ─────────────────────────────────────────────

/// HookRuntime 的具体实现：线程安全的注册表 + 分发。
pub struct DefaultHookRuntime {
    registrations: RwLock<Vec<RegisteredHook>>,
    http_policy: crate::hooks::http::HttpHookPolicy,
    evaluator: Option<Arc<dyn HookEvaluator>>,
    background_tx: tokio::sync::broadcast::Sender<crate::hooks::command::BackgroundHookEvent>,
    background_tasks: crate::hooks::command::BackgroundTasks,
}

impl DefaultHookRuntime {
    pub fn new() -> Self {
        Self::with_http_policy(Default::default())
    }

    pub fn with_http_policy(http_policy: crate::hooks::http::HttpHookPolicy) -> Self {
        let (background_tx, _) = tokio::sync::broadcast::channel(64);
        Self {
            registrations: RwLock::new(Vec::new()),
            http_policy,
            evaluator: None,
            background_tx,
            background_tasks: Default::default(),
        }
    }

    /// 注入 Prompt/Agent Hook 所需的模型能力。
    pub fn with_evaluator(mut self, evaluator: Arc<dyn HookEvaluator>) -> Self {
        self.evaluator = Some(evaluator);
        self
    }

    fn compile(&self, reg: HookRegistration) -> Result<RegisteredHook, YourAiError> {
        let config_identity =
            Some(serde_json::to_string(&reg.handler).map_err(|e| {
                ErrorKind::Config(format!("cannot identify hook configuration: {e}"))
            })?);
        let if_condition = reg.handler.if_condition().map(ToOwned::to_owned);
        let status_message = reg.handler.status_message().map(ToOwned::to_owned);
        let handler = handler_from_config(
            &reg.handler,
            self.http_policy.clone(),
            self.evaluator.clone(),
            crate::hooks::command::BackgroundCommandContext {
                tasks: self.background_tasks.clone(),
                hook_id: reg.id.clone(),
                rewake: reg.handler.async_rewake(),
                timeout: reg.timeout,
                force_background: reg.handler.is_async(),
                sender: self.background_tx.clone(),
            },
        )?;
        Ok(RegisteredHook {
            id: reg.id,
            event: reg.event,
            matcher: reg.matcher,
            handler,
            config_identity,
            timeout: reg.timeout,
            source: reg.source,
            failure_policy: reg.failure_policy,
            once: reg.once,
            if_condition,
            status_message,
        })
    }

    /// 从配置批量注册。
    pub async fn register_config(
        &self,
        config: &HooksConfig,
        source: HookSource,
    ) -> Result<(), YourAiError> {
        let regs =
            crate::hooks::config::build_registrations(config, source).map_err(ErrorKind::Config)?;
        let compiled = regs
            .into_iter()
            .map(|reg| self.compile(reg))
            .collect::<Result<Vec<_>, _>>()?;
        self.registrations.write().await.extend(compiled);
        Ok(())
    }

    /// 测试辅助：注入已经构造好的注册项。生产代码走 `register_config` 或 `HookRegistry`。
    #[cfg(test)]
    async fn register(&self, reg: HookRegistration) {
        let reg = self.compile(reg).expect("test registration must compile");
        let mut guard = self.registrations.write().await;
        guard.push(reg);
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
            let mut guard = self.registrations.write().await;
            // Config-sourced handlers dedup by (source, serialized config);
            // the first occurrence wins, matching registration semantics.
            let mut seen_config_handlers: std::collections::HashSet<(String, String)> =
                std::collections::HashSet::new();
            let mut matched = Vec::new();
            guard.retain(|reg| {
                let matches = reg.event == event
                    && condition_matches(reg.if_condition.as_deref(), invocation)
                    && match match_query {
                        Some(q) => reg.matcher.matches(q),
                        None => true,
                    };
                let selected = matches
                    && reg.config_identity.as_ref().is_none_or(|identity| {
                        seen_config_handlers
                            .insert((reg.source.as_str().to_owned(), identity.clone()))
                    });
                if selected {
                    matched.push(reg.clone());
                }
                // Claim before scheduling: failures and cancelled dispatches consume once.
                !(selected && reg.once)
            });
            matched
        };

        if matched.is_empty() {
            return Ok(HookDispatchResult::empty(event));
        }

        let results = run_handlers_parallel(&matched, invocation).await;

        // 执行记录保留完成顺序用于观测；聚合贡献必须恢复注册顺序，确保语义确定。
        let mut indexed_contributions = Vec::new();
        let mut runs = Vec::new();

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
                            indexed_contributions.push((executed.registration_index, contrib));
                            runs.push(run);
                        }
                        Err(e) => {
                            run.status = HookRunStatus::Failed;
                            run.stderr = Some(e.to_string());
                            indexed_contributions.push((
                                executed.registration_index,
                                failure_contribution(reg, e.to_string()),
                            ));
                            runs.push(run);
                        }
                    }
                }
                HandlerExecResult::Failed { error, duration } => {
                    run.duration = duration;
                    run.status = HookRunStatus::Failed;
                    run.stderr = Some(error.clone());
                    indexed_contributions.push((
                        executed.registration_index,
                        failure_contribution(reg, error),
                    ));
                    runs.push(run);
                }
                HandlerExecResult::TimedOut { duration } => {
                    run.duration = duration;
                    run.status = HookRunStatus::TimedOut;
                    indexed_contributions.push((
                        executed.registration_index,
                        failure_contribution(reg, format!("hook timed out after {duration:?}")),
                    ));
                    runs.push(run);
                }
            }
        }

        indexed_contributions.sort_by_key(|(registration_index, _)| *registration_index);
        let contributions: Vec<Contribution> = indexed_contributions
            .into_iter()
            .map(|(_, contribution)| contribution)
            .collect();
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

impl Default for DefaultHookRuntime {
    fn default() -> Self {
        Self::new()
    }
}

impl HookRuntime for DefaultHookRuntime {
    fn subscribe_background(
        &self,
    ) -> Option<tokio::sync::broadcast::Receiver<yourai_core::hooks::HookBackgroundEvent>> {
        Some(self.background_tx.subscribe())
    }
    fn shutdown_session<'a>(
        &'a self,
        id: &'a str,
    ) -> yourai_core::BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let tasks = self
                .background_tasks
                .lock()
                .unwrap()
                .remove(id)
                .unwrap_or_default();
            for task in &tasks {
                task.abort();
            }
            for task in tasks {
                let _ = task.await;
            }
            Ok(())
        })
    }

    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> yourai_core::BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(self.dispatch_inner(invocation))
    }
}

impl HookRegistry for DefaultHookRuntime {
    fn register<'a>(
        &'a self,
        registration: NativeHookRegistration,
    ) -> yourai_core::BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let reg = RegisteredHook {
                id: registration.id,
                event: registration.event,
                matcher: match registration.matcher.as_deref() {
                    Some(pattern) => crate::hooks::matcher::CompiledMatcher::try_compile(pattern)
                        .map_err(ErrorKind::Config)?,
                    None => crate::hooks::matcher::CompiledMatcher::All,
                },
                handler: registration.handler,
                config_identity: None,
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
        return crate::hooks::matcher::normalize_legacy_tool_name(condition)
            == crate::hooks::matcher::normalize_legacy_tool_name(tool_name);
    };
    if !condition.ends_with(')')
        || crate::hooks::matcher::normalize_legacy_tool_name(condition[..open].trim())
            != crate::hooks::matcher::normalize_legacy_tool_name(tool_name)
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
    crate::hooks::matcher::cached_regex(&regex).is_some_and(|re| re.is_match(candidate))
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
) -> Vec<ExecutedHook> {
    let mut join_set: JoinSet<ExecutedHook> = JoinSet::new();
    let mut task_metadata = HashMap::new();

    for (registration_index, reg) in matched.iter().enumerate() {
        let inv = invocation.clone();
        let timeout = reg.timeout;
        let handler = reg.handler.clone();

        let started_at = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_millis() as i64;
        let task = join_set.spawn(async move {
            let start = std::time::Instant::now();
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
        task_metadata.insert(task.id(), (registration_index, started_at));
    }

    let mut results = Vec::with_capacity(matched.len());
    while let Some(res) = join_set.join_next().await {
        match res {
            Ok(executed) => results.push(executed),
            Err(error) => {
                if let Some((registration_index, started_at)) = task_metadata.get(&error.id()) {
                    results.push(ExecutedHook {
                        registration_index: *registration_index,
                        started_at: *started_at,
                        result: HandlerExecResult::Failed {
                            error: format!("hook handler task failed: {error}"),
                            duration: Duration::ZERO,
                        },
                    });
                }
            }
        }
    }
    results
}

impl Drop for DefaultHookRuntime {
    fn drop(&mut self) {
        for tasks in self.background_tasks.lock().unwrap().values() {
            for task in tasks {
                task.abort();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::hooks::config::HandlerConfig;
    use crate::hooks::handler::{HookEvaluator, HookModelDecision, HookModelRequest};
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;
    use tokio::sync::Notify;
    use yourai_core::hooks::{BaseInput, HookEvent};
    use yourai_core::BoxFuture;

    struct RecordingEvaluator {
        requests: Mutex<Vec<HookModelRequest>>,
        decision: HookModelDecision,
    }

    impl HookEvaluator for RecordingEvaluator {
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
        matcher: crate::hooks::matcher::CompiledMatcher,
        command: &str,
    ) -> HookRegistration {
        HookRegistration {
            id: id.to_string(),
            event: yourai_core::hooks::HookEventKind::from_name(event_name).unwrap(),
            matcher,
            handler: crate::hooks::config::HandlerConfig::Command {
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
        let rt = DefaultHookRuntime::new();
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
    async fn prompt_and_agent_handlers_use_injected_evaluator() {
        let executor = Arc::new(RecordingEvaluator {
            requests: Mutex::new(Vec::new()),
            decision: HookModelDecision {
                ok: true,
                reason: None,
            },
        });
        let rt = DefaultHookRuntime::new().with_evaluator(executor.clone());
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
    async fn prompt_handler_requires_evaluator_at_registration() {
        let rt = DefaultHookRuntime::new();
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
        assert!(error.to_string().contains("HookEvaluator"));
    }

    #[tokio::test]
    async fn failed_configuration_compilation_keeps_existing_registrations() {
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "existing",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
            "printf existing",
        ))
        .await;
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {
                "UserPromptSubmit": [{"hooks": [
                    {"type": "command", "command": "printf new"},
                    {"type": "agent", "prompt": "review"}
                ]}]
            }
        }))
        .unwrap();
        let error = rt
            .register_config(&config, HookSource::Project)
            .await
            .unwrap_err();
        assert!(error.to_string().contains("HookEvaluator"));
        assert_eq!(rt.handler_ids().await, ["existing"]);
    }

    #[tokio::test]
    async fn rejected_prompt_handler_blocks_with_reason() {
        let executor = Arc::new(RecordingEvaluator {
            requests: Mutex::new(Vec::new()),
            decision: HookModelDecision {
                ok: false,
                reason: Some("model rejected prompt".to_string()),
            },
        });
        let rt = DefaultHookRuntime::new().with_evaluator(executor);
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
    async fn rejected_pretool_prompt_denies_tool_without_stopping_turn() {
        let executor = Arc::new(RecordingEvaluator {
            requests: Mutex::new(Vec::new()),
            decision: HookModelDecision {
                ok: false,
                reason: Some("tool rejected by model hook".to_string()),
            },
        });
        let rt = DefaultHookRuntime::new().with_evaluator(executor);
        let config: HooksConfig = serde_json::from_value(serde_json::json!({
            "hooks": {
                "PreToolUse": [{
                    "matcher": "Bash",
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
                HookEvent::PreToolUse {
                    tool_name: "Bash".to_string(),
                    tool_input: serde_json::json!({"command": "rm -rf /tmp/example"}),
                    tool_use_id: "call".to_string(),
                },
            ))
            .await
            .unwrap();

        assert!(!result.common.prevent_continuation);
        match result.outcome {
            HookPointOutcome::PreToolUse(outcome) => assert!(matches!(
                outcome.permission,
                HookPermission::Deny { ref reason } if reason == "tool rejected by model hook"
            )),
            _ => panic!("expected PreToolUse outcome"),
        }
    }

    #[tokio::test]
    async fn dispatch_command_handler_echo_context() {
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "echo-1",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "duplicate-1",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
            "printf once",
        ))
        .await;
        rt.register(command_registration(
            "duplicate-2",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "block-1",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "allow-1",
            "PreToolUse",
            crate::hooks::matcher::CompiledMatcher::compile("Bash"),
            r#"printf '{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"allow"}}'"#,
        ))
        .await;
        rt.register(command_registration(
            "deny-1",
            "PreToolUse",
            crate::hooks::matcher::CompiledMatcher::compile("Bash"),
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "mismatch-1",
            "PreToolUse",
            crate::hooks::matcher::CompiledMatcher::compile("Bash"),
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "slow",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
            "sleep 0.05; printf slow",
        ))
        .await;
        rt.register(command_registration(
            "fast",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
    async fn aggregation_uses_registration_order_not_completion_order() {
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "slow-first",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
            r#"sleep 0.05; printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"first"}}'"#,
        ))
        .await;
        rt.register(command_registration(
            "fast-second",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
            r#"printf '{"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"second"}}'"#,
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

        assert_eq!(result.runs[0].hook_id, "fast-second");
        match result.outcome {
            HookPointOutcome::UserPromptSubmit(outcome) => {
                assert_eq!(outcome.additional_contexts, ["first", "second"]);
            }
            _ => panic!("expected UserPromptSubmit outcome"),
        }
    }

    #[tokio::test]
    async fn plain_text_is_message_not_additional_context() {
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "plain",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        rt.register(command_registration(
            "common",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
    async fn once_hook_is_consumed_by_execution() {
        let rt = DefaultHookRuntime::new();
        let mut registration = command_registration(
            "once",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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

    struct WaitingHandler {
        calls: AtomicUsize,
        started: Notify,
        release: Notify,
    }
    impl WaitingHandler {
        fn new() -> Arc<Self> {
            Arc::new(Self {
                calls: AtomicUsize::new(0),
                started: Notify::new(),
                release: Notify::new(),
            })
        }
    }
    impl HookHandler for WaitingHandler {
        fn execute<'a>(
            &'a self,
            _: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
            Box::pin(async move {
                self.calls.fetch_add(1, Ordering::SeqCst);
                self.started.notify_one();
                self.release.notified().await;
                Ok(HookOutput::Parsed(serde_json::json!({})))
            })
        }
    }
    fn prompt_invocation(session: &str) -> HookInvocation {
        HookInvocation::new(
            BaseInput::new(session, "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".into(),
            },
        )
    }
    fn native_once(
        handler: Arc<dyn HookHandler>,
        timeout: Option<Duration>,
    ) -> NativeHookRegistration {
        NativeHookRegistration {
            id: "once".into(),
            event: yourai_core::hooks::HookEventKind::UserPromptSubmit,
            matcher: None,
            handler,
            timeout,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Open,
            once: true,
        }
    }

    #[tokio::test]
    async fn concurrent_dispatches_claim_once_before_handler_finishes() {
        let rt = Arc::new(DefaultHookRuntime::new());
        let handler = WaitingHandler::new();
        HookRegistry::register(rt.as_ref(), native_once(handler.clone(), None))
            .await
            .unwrap();
        let first_runtime = rt.clone();
        let first =
            tokio::spawn(async move { first_runtime.dispatch(&prompt_invocation("first")).await });
        handler.started.notified().await;
        assert!(rt
            .dispatch(&prompt_invocation("second"))
            .await
            .unwrap()
            .runs
            .is_empty());
        assert!(rt.handler_ids().await.is_empty());
        handler.release.notify_one();
        assert_eq!(first.await.unwrap().unwrap().runs.len(), 1);
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn cancelled_dispatch_does_not_restore_a_claimed_once_handler() {
        let rt = Arc::new(DefaultHookRuntime::new());
        let handler = WaitingHandler::new();
        HookRegistry::register(rt.as_ref(), native_once(handler.clone(), None))
            .await
            .unwrap();
        let first_runtime = rt.clone();
        let first =
            tokio::spawn(async move { first_runtime.dispatch(&prompt_invocation("first")).await });
        handler.started.notified().await;
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        assert!(rt
            .dispatch(&prompt_invocation("second"))
            .await
            .unwrap()
            .runs
            .is_empty());
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn timed_out_once_handler_is_consumed() {
        let rt = DefaultHookRuntime::new();
        let handler = WaitingHandler::new();
        HookRegistry::register(
            &rt,
            native_once(handler.clone(), Some(Duration::from_secs(1))),
        )
        .await
        .unwrap();
        let result = rt.dispatch(&prompt_invocation("first")).await.unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::TimedOut);
        assert!(rt
            .dispatch(&prompt_invocation("second"))
            .await
            .unwrap()
            .runs
            .is_empty());
        assert_eq!(handler.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn failed_once_handler_does_not_remove_another_registration_with_the_same_id() {
        let rt = DefaultHookRuntime::new();
        let failing = crate::hooks::handler::NativeHandler::new(|_| Err("failed once".into()));
        HookRegistry::register(&rt, native_once(Arc::new(failing), None))
            .await
            .unwrap();
        let mut remaining = native_once(
            Arc::new(crate::hooks::handler::NativeHandler::new(|_| {
                Ok(HookJsonOutput::Sync(Default::default()))
            })),
            None,
        );
        remaining.event = yourai_core::hooks::HookEventKind::Stop;
        remaining.once = false;
        HookRegistry::register(&rt, remaining).await.unwrap();
        let result = rt.dispatch(&prompt_invocation("first")).await.unwrap();
        assert_eq!(result.runs[0].status, HookRunStatus::Failed);
        assert_eq!(rt.handler_ids().await, ["once"]);
        assert!(rt
            .dispatch(&prompt_invocation("second"))
            .await
            .unwrap()
            .runs
            .is_empty());
        let stop = HookInvocation::new(
            BaseInput::new("second", "/tmp"),
            HookEvent::Stop {
                stop_hook_active: false,
                last_assistant_message: None,
            },
        );
        assert_eq!(rt.dispatch(&stop).await.unwrap().runs.len(), 1);
        assert!(HookRegistry::unregister(&rt, "once").await.unwrap());
        assert!(!HookRegistry::unregister(&rt, "once").await.unwrap());
    }

    #[tokio::test]
    async fn compiled_background_command_uses_each_invocations_session() {
        for command in ["printf done", "printf '{\"async\":true}\\n'; printf done"] {
            let rt = DefaultHookRuntime::new();
            let mut events = rt.subscribe_background().unwrap();
            let mut registration = command_registration(
                "shared",
                "UserPromptSubmit",
                crate::hooks::matcher::CompiledMatcher::All,
                command,
            );
            if command == "printf done" {
                if let HandlerConfig::Command { is_async, .. } = &mut registration.handler {
                    *is_async = Some(true);
                }
            }
            rt.register(registration).await;
            for session in ["first", "second"] {
                let result = rt.dispatch(&prompt_invocation(session)).await.unwrap();
                assert_eq!(result.runs[0].status, HookRunStatus::Backgrounded);
            }
            let mut sessions = Vec::new();
            for _ in 0..2 {
                let event = tokio::time::timeout(Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.event_name, "UserPromptSubmit");
                assert_eq!(event.hook_id, "shared");
                assert_eq!(event.stdout, "done");
                sessions.push(event.session_id);
            }
            sessions.sort();
            assert_eq!(sessions, ["first", "second"]);
        }
    }

    #[tokio::test]
    async fn if_condition_filters_before_spawn() {
        let rt = DefaultHookRuntime::new();
        let mut registration = command_registration(
            "git-only",
            "PreToolUse",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        let mut receiver = rt.subscribe_background().unwrap();
        let mut registration = command_registration(
            "async",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        let mut receiver = rt.subscribe_background().unwrap();
        rt.register(command_registration(
            "handshake",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        let mut receiver = rt.subscribe_background().unwrap();
        rt.register(command_registration(
            "handshake-timeout",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        let mut registration = command_registration(
            "closed",
            "UserPromptSubmit",
            crate::hooks::matcher::CompiledMatcher::All,
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
        let rt = DefaultHookRuntime::new();
        let handler = crate::hooks::handler::NativeHandler::new(|_| {
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

    #[tokio::test]
    async fn panicking_native_handler_produces_failed_run() {
        let rt = DefaultHookRuntime::new();
        let handler =
            crate::hooks::handler::NativeHandler::new(|_| -> Result<HookJsonOutput, String> {
                panic!("native hook panic")
            });
        HookRegistry::register(
            &rt,
            NativeHookRegistration {
                id: "panicking-native".to_string(),
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
        assert_eq!(result.runs.len(), 1);
        assert_eq!(result.runs[0].status, HookRunStatus::Failed);
        assert!(result.runs[0]
            .stderr
            .as_deref()
            .unwrap()
            .contains("hook handler task failed"));
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
