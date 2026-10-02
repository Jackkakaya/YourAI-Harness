#[path = "support/execution.rs"]
mod custom;
#[path = "support/loop.rs"]
mod support;
use custom::{DirectLoop, ReverseLoop};
use serde_json::json;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc,
};
use support::*;
use yourai_core::{
    hooks::{PermissionRequestBehavior, PermissionRequestDecision},
    prelude::*,
};

#[tokio::test]
async fn custom_scheduling_uses_wrapped_calls_and_can_change_order() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![
        calls(&["first", "second"]),
        answer("done"),
    ]));
    let first = Arc::new(Handler::new("first", Mode::Return));
    let second = Arc::new(Handler::new("second", Mode::Fail));
    let registry = Arc::new(Registry::default());
    registry.register(first.clone());
    registry.register(second.clone());
    let hooks = Arc::new(Hooks::new(|i, result| {
        if let HookPointOutcome::PreToolUse(outcome) = &mut result.outcome {
            outcome.updated_input = Some(json!({"value":7}));
            outcome.additional_contexts.push("pre context".into());
        }
        if matches!(i.event, HookEvent::PostToolUse { .. }) {
            if let HookPointOutcome::PostToolUse(outcome) = &mut result.outcome {
                outcome.additional_contexts.push("post context".into());
            }
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(ReverseLoop))
        .model(model.clone())
        .context_manager(history.clone())
        .tools(registry)
        .hooks(hooks.clone())
        .build();
    let (events, result) = collect(agent.start(In::user_text("go")).unwrap()).await;
    assert_eq!(result.unwrap().text, "done");
    assert_eq!(*first.inputs.lock().unwrap(), [json!({"value":7})]);
    assert_eq!(*second.inputs.lock().unwrap(), [json!({"value":7})]);
    let started: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Out::ToolStarted { name, .. } => Some(name.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(started, ["second", "first"]);
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUseFailure,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Stop
        ]
    );
    let requests = model.requests.lock().unwrap();
    let contexts = requests[1]
        .request
        .messages
        .iter()
        .flat_map(|m| m.content.texts())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(contexts.contains("pre context"));
    assert!(contexts.contains("post context"));
}

fn direct(
    program: Arc<DirectLoop>,
    history: Arc<History>,
    tool: Arc<Handler>,
    hooks: Arc<Hooks>,
) -> Arc<Agent> {
    let registry = Arc::new(Registry::default());
    registry.register(tool);
    // Programmatic tools need no model provider or direct handler access.
    Agent::builder()
        .agent_loop(program)
        .context_manager(history)
        .tools(registry)
        .hooks(hooks)
        .build()
}
fn direct_program() -> Arc<DirectLoop> {
    Arc::new(DirectLoop {
        starts: Arc::new(AtomicUsize::new(0)),
        runs: Arc::new(AtomicUsize::new(0)),
    })
}

#[tokio::test]
async fn stop_continuation_resumes_same_business_instance_without_replaying_tool() {
    let program = direct_program();
    let tool = Arc::new(Handler::new("tool", Mode::Return));
    let history = Arc::new(History::default());
    let stops = Arc::new(AtomicUsize::new(0));
    let count = stops.clone();
    let hooks = Arc::new(Hooks::new(move |i, result| {
        if let HookEvent::Stop {
            stop_hook_active, ..
        } = i.event
        {
            let n = count.fetch_add(1, Ordering::SeqCst);
            assert_eq!(stop_hook_active, n > 0);
            if n == 0 {
                block(result);
            }
        }
    }));
    let result = direct(program.clone(), history.clone(), tool.clone(), hooks)
        .run(In::user_text("go"))
        .await
        .unwrap();
    assert_eq!(result.text, "business complete");
    assert_eq!(program.starts.load(Ordering::SeqCst), 1);
    assert_eq!(program.runs.load(Ordering::SeqCst), 2);
    assert_eq!(tool.inputs.lock().unwrap().len(), 1);
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Assistant
                && m.content.texts().join("") == "business complete")
            .count(),
        1
    );
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count(),
        1
    );
}

#[tokio::test]
async fn rejected_input_never_enters_business_loop() {
    let program = direct_program();
    let tool = Arc::new(Handler::new("tool", Mode::Return));
    let history = Arc::new(History::default());
    let hooks = Arc::new(Hooks::new(|i, result| {
        if matches!(i.event, HookEvent::UserPromptSubmit { .. }) {
            block(result);
        }
    }));
    let output = direct(program.clone(), history.clone(), tool.clone(), hooks)
        .run(In::user_text("go"))
        .await
        .unwrap();
    assert_eq!(output.rejected.len(), 1);
    assert_eq!(program.runs.load(Ordering::SeqCst), 0);
    assert!(tool.inputs.lock().unwrap().is_empty());
    assert!(history.messages().is_empty());
}

#[tokio::test]
async fn custom_loop_permission_deny_does_not_execute_backend() {
    let program = direct_program();
    let tool = Arc::new(Handler::new("tool", Mode::Return));
    let hooks = Arc::new(Hooks::new(|_, result| {
        if let HookPointOutcome::PreToolUse(outcome) = &mut result.outcome {
            outcome.permission = HookPermission::Deny {
                reason: "blocked".into(),
            };
        }
    }));
    let history = Arc::new(History::default());
    let result = direct(program, history.clone(), tool.clone(), hooks.clone())
        .run(In::user_text("go"))
        .await
        .unwrap();
    assert_eq!(result.text, "business complete");
    assert!(tool.inputs.lock().unwrap().is_empty());
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PermissionDenied));
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PostToolUseFailure));
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count(),
        1
    );
}

#[tokio::test]
async fn custom_loop_mcp_interaction_is_wrapped_without_loop_hook_code() {
    let tool = Arc::new(Handler::new("tool", Mode::Mcp));
    let hooks = Arc::new(Hooks::new(|_, result| match &mut result.outcome {
        HookPointOutcome::Elicitation(outcome) => {
            outcome.action = Some("accept".into());
            outcome.content = Some(json!({"choice":"before"}));
        }
        HookPointOutcome::ElicitationResult(outcome) => {
            outcome.content = Some(json!({"choice":"after"}));
        }
        _ => (),
    }));
    let (events, result) = collect(
        direct(
            direct_program(),
            Arc::new(History::default()),
            tool,
            hooks.clone(),
        )
        .start(In::user_text("go"))
        .unwrap(),
    )
    .await;
    assert!(result.is_ok());
    assert!(!events.iter().any(|e| matches!(e, Out::Ask { .. })));
    assert!(events.iter().any(
        |e| matches!(e, Out::ToolDone { output, .. } if output["content"]["choice"] == "after")
    ));
    let seen = hooks.seen.lock().unwrap();
    assert!(seen.contains(&HookEventKind::Elicitation));
    assert!(seen.contains(&HookEventKind::ElicitationResult));
}

#[tokio::test]
async fn custom_scheduling_model_failure_reports_stop_failure_once() {
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = Agent::builder()
        .agent_loop(Arc::new(ReverseLoop))
        .context_manager(Arc::new(History::default()))
        .model(Arc::new(Model::new(vec![error_stream()])))
        .hooks(hooks.clone())
        .build();
    assert!(agent.run(In::user_text("go")).await.is_err());
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|e| **e == HookEventKind::StopFailure)
            .count(),
        1
    );
    assert!(!hooks.seen.lock().unwrap().contains(&HookEventKind::Stop));
}

#[tokio::test(start_paused = true)]
async fn cancellation_bounds_a_custom_loop_wait() {
    let entered = Arc::new(AtomicUsize::new(0));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::WaitingLoop(entered.clone())))
        .context_manager(Arc::new(History::default()))
        .build();
    let handle = agent.start(In::user_text("go")).unwrap();
    while entered.load(Ordering::SeqCst) == 0 {
        tokio::task::yield_now().await;
    }
    handle.cancel.cancel();
    let output = handle.join().await.unwrap_err();
    assert!(matches!(
        *output.error,
        YourAiError::Aborted(AbortReason::Cancelled)
    ));
}

#[tokio::test]
async fn harness_configuration_keeps_the_custom_loop_and_lifecycle_hooks() {
    use yourai_harness::{Harness, HarnessConfig};
    let dir = tempfile::tempdir().unwrap();
    let program = direct_program();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.agent_loop = Some(program.clone());
    config.yolo = true;
    config.system_prompt = Some("test".into());
    let harness = Harness::open(config, Arc::new(Model::new(vec![])))
        .await
        .unwrap();
    let tool = Arc::new(Handler::new("tool", Mode::Return));
    harness.tools.register(tool.clone());
    harness
        .host
        .workspace()
        .unwrap()
        .change_config("project_settings", json!({"memory_search_limit":0}))
        .await
        .unwrap();
    let observed = Arc::new(std::sync::Mutex::new(vec![]));
    for event in [
        HookEventKind::UserPromptSubmit,
        HookEventKind::PreToolUse,
        HookEventKind::PostToolUse,
        HookEventKind::Stop,
        HookEventKind::TurnCompleted,
        HookEventKind::SessionEnd,
    ] {
        let observed = observed.clone();
        let handler = Arc::new(yourai_harness::hooks::NativeHandler::new(
            move |invocation| {
                observed.lock().unwrap().push(invocation.event_kind());
                Ok::<_, String>(yourai_harness::hooks::wire_output::HookJsonOutput::Sync(
                    Default::default(),
                ))
            },
        ));
        HookRegistry::register(
            harness.hooks.as_ref(),
            NativeHookRegistration {
                id: event.as_str().into(),
                event,
                matcher: None,
                handler,
                timeout: None,
                source: HookSource::Session,
                failure_policy: FailurePolicy::Open,
                once: false,
            },
        )
        .await
        .unwrap();
    }
    harness
        .host
        .submit_async(In::user_text("go"))
        .await
        .unwrap();
    let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
    let report = harness
        .host
        .run_next(
            TurnLimits::default(),
            &tx,
            &tokio_util::sync::CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.result.unwrap().text, "business complete");
    assert_eq!(program.runs.load(Ordering::SeqCst), 1);
    assert_eq!(tool.inputs.lock().unwrap().len(), 1);
    harness.close().await.unwrap();
    assert_eq!(
        *observed.lock().unwrap(),
        [
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Stop,
            HookEventKind::TurnCompleted,
            HookEventKind::SessionEnd
        ]
    );
}

#[tokio::test]
async fn replacement_context_summary_gets_hooks_and_preserves_commit_after_post_stop() {
    let history = Arc::new(History::default());
    history.tokens.store(100, Ordering::SeqCst);
    let hooks = Arc::new(Hooks::new(|i, result| {
        if matches!(i.event, HookEvent::PreCompact { .. }) {
            if let HookPointOutcome::Generic(outcome) = &mut result.outcome {
                outcome.additional_contexts.push("retain decisions".into());
            }
        }
        if matches!(i.event, HookEvent::PostCompact { .. }) {
            result.common.prevent_continuation = true;
            result.common.stop_reason = Some("stop after commit".into());
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::CompactLoop))
        .context_manager(history.clone())
        .model(Arc::new(Model::new(vec![])))
        .hooks(hooks.clone())
        .build();
    let snapshot = agent.ctx().snapshot().unwrap();
    let execution = ContextExecution::from_snapshot(&snapshot, history.session_id(), None).unwrap();
    let mut request = CompactionRequest::new(CompactionTrigger::Manual);
    request.custom_instructions = Some("original instructions".into());
    let result = yourai_harness::context::compact(
        history.as_ref(),
        request,
        &execution,
        &tokio_util::sync::CancellationToken::new(),
        None,
    )
    .await
    .unwrap();
    assert_eq!(result.action, CompactAction::Summarized);
    assert_eq!(result.stop_reason.as_deref(), Some("stop after commit"));
    assert_eq!(history.tokens.load(Ordering::SeqCst), 1);
    assert_eq!(
        *history.summary_instructions.lock().unwrap(),
        [Some("original instructions\nretain decisions".into())]
    );
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [HookEventKind::PreCompact, HookEventKind::PostCompact]
    );
}

#[tokio::test]
async fn replacement_context_pre_stop_does_not_run_summary_backend() {
    let history = Arc::new(History::default());
    history.tokens.store(100, Ordering::SeqCst);
    let hooks = Arc::new(Hooks::new(|i, result| {
        if matches!(i.event, HookEvent::PreCompact { .. }) {
            block(result);
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::CompactLoop))
        .context_manager(history.clone())
        .model(Arc::new(Model::new(vec![])))
        .hooks(hooks.clone())
        .build();
    assert!(agent.run(In::user_text("compact")).await.is_err());
    assert!(history.compactions.lock().unwrap().is_empty());
    assert!(history.summary_instructions.lock().unwrap().is_empty());
    assert_eq!(history.tokens.load(Ordering::SeqCst), 100);
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [HookEventKind::UserPromptSubmit, HookEventKind::PreCompact]
    );
}

#[tokio::test]
async fn replacement_context_noop_and_prune_do_not_emit_summary_hooks() {
    for action in [CompactAction::Unchanged, CompactAction::Pruned] {
        let history = Arc::new(History::default());
        *history.compaction_action.lock().unwrap() = Some(action);
        let hooks = Arc::new(Hooks::new(|_, _| {}));
        let agent = Agent::builder()
            .agent_loop(Arc::new(custom::CompactLoop))
            .context_manager(history.clone())
            .model(Arc::new(Model::new(vec![])))
            .hooks(hooks.clone())
            .build();
        assert_eq!(
            agent.run(In::user_text("compact")).await.unwrap().text,
            "compacted"
        );
        assert_eq!(
            *hooks.seen.lock().unwrap(),
            [HookEventKind::UserPromptSubmit, HookEventKind::Stop]
        );
        assert!(history.summary_instructions.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn registry_changes_during_pre_hook_do_not_replace_a_bound_backend() {
    let original = Arc::new(Handler::new("tool", Mode::Return));
    let replacement = Arc::new(Handler::new("tool", Mode::Fail));
    let registry = Arc::new(Registry::default());
    registry.register(original.clone());
    let updated_registry = registry.clone();
    let updated_handler = replacement.clone();
    let hooks = Arc::new(Hooks::new(move |i, _| {
        if matches!(i.event, HookEvent::PreToolUse { .. }) {
            updated_registry.register(updated_handler.clone());
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(ReverseLoop))
        .context_manager(Arc::new(History::default()))
        .model(Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])))
        .tools(registry)
        .hooks(hooks.clone())
        .build();
    assert_eq!(agent.run(In::user_text("go")).await.unwrap().text, "done");
    assert_eq!(original.inputs.lock().unwrap().len(), 1);
    assert!(replacement.inputs.lock().unwrap().is_empty());
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PostToolUse));
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PostToolUseFailure));
}

#[tokio::test]
async fn independent_input_permission_and_interaction_operations_apply_hooks() {
    let history = Arc::new(History::default());
    let handler = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(handler.clone());
    let requests = Arc::new(AtomicUsize::new(0));
    let seen_requests = requests.clone();
    let hooks = Arc::new(Hooks::new(move |i, result| {
        if matches!(&i.event, HookEvent::UserPromptSubmit { prompt } if prompt == "reject this") {
            block(result);
        }
        match &mut result.outcome {
            HookPointOutcome::PermissionRequest(o) => {
                let first = seen_requests.fetch_add(1, Ordering::SeqCst) == 0;
                o.decision = Some(PermissionRequestDecision {
                    behavior: if first {
                        PermissionRequestBehavior::Deny
                    } else {
                        PermissionRequestBehavior::Allow
                    },
                    updated_input: (!first).then(|| json!({"value":2})),
                    updated_permissions: vec![],
                    message: None,
                    interrupt: false,
                });
            }
            HookPointOutcome::PermissionDenied(o) => o.retry = true,
            HookPointOutcome::Elicitation(o) => {
                o.action = Some("accept".into());
                o.content = Some(json!({"choice":"before"}));
            }
            HookPointOutcome::ElicitationResult(o) => o.content = Some(json!({"choice":"after"})),
            _ => (),
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::OperationsLoop))
        .context_manager(history.clone())
        .tools(registry)
        .security(Arc::new(Security::new(ApprovalDecision::Ask)))
        .hooks(hooks.clone())
        .build();
    let (events, result) = collect(agent.start(In::user_text("initial")).unwrap()).await;
    let output = result.unwrap();
    assert_eq!(output.text, "operations complete");
    assert_eq!(output.rejected.len(), 1);
    assert!(handler.inputs.lock().unwrap().is_empty());
    assert!(!events
        .iter()
        .any(|e| matches!(e, Out::Ask { .. } | Out::ToolStarted { .. })));
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .count(),
        2
    );
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            HookEventKind::UserPromptSubmit,
            HookEventKind::UserPromptSubmit,
            HookEventKind::UserPromptSubmit,
            HookEventKind::PermissionRequest,
            HookEventKind::PermissionDenied,
            HookEventKind::PermissionRequest,
            HookEventKind::Elicitation,
            HookEventKind::ElicitationResult,
            HookEventKind::Stop
        ]
    );
}

#[tokio::test]
async fn public_registry_binding_exec_uses_framework_lifecycle_and_rejects_replay() {
    let tool = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(tool.clone());
    let hooks = Arc::new(Hooks::new(|_, result| {
        if let HookPointOutcome::PreToolUse(o) = &mut result.outcome {
            o.updated_input = Some(json!({"value":5}));
        }
    }));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::BoundLoop {
            replacement: Arc::new(Handler::new("tool", Mode::Fail)),
        }))
        .context_manager(Arc::new(History::default()))
        .tools(registry)
        .hooks(hooks.clone())
        .model(Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])))
        .build();
    assert_eq!(agent.run(In::user_text("go")).await.unwrap().text, "done");
    assert_eq!(*tool.inputs.lock().unwrap(), [json!({"value":5})]);
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreToolUse,
            HookEventKind::PostToolUse,
            HookEventKind::Stop
        ]
    );
}

#[tokio::test]
async fn unresolved_tool_calls_block_dependent_operations() {
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("tool", Mode::Return)));
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::GuardLoop))
        .context_manager(Arc::new(History::default()))
        .tools(registry)
        .hooks(Arc::new(Hooks::new(|_, _| {})))
        .model(Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])))
        .build();
    assert_eq!(agent.run(In::user_text("go")).await.unwrap().text, "done");
}
