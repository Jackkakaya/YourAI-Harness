mod support;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};
use support::*;
use yourai_core::{hooks::*, model::ModelRecovery, prelude::*};
use yourai_loop::LoopConfig;

#[tokio::test]
async fn streamed_answer_commits_once_and_usage_is_delta() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![answer("hello")]));
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = builder(model.clone(), history.clone(), LoopConfig::default())
        .hooks(hooks.clone())
        .build();
    let (events, result) = collect(agent.start(In::user_text("hi")).unwrap()).await;
    let result = result.unwrap();
    assert_eq!(result.text, "hello");
    assert_eq!(result.usage.unwrap().total_tokens, 3);
    assert_eq!(history.messages().len(), 2);
    assert_eq!(
        hooks.seen.lock().unwrap().as_slice(),
        [HookEventKind::UserPromptSubmit, HookEventKind::Stop]
    );
    assert_eq!(
        events
            .iter()
            .filter(|e| matches!(e, Out::Message { .. }))
            .count(),
        1
    );
    assert_eq!(
        model.requests.lock().unwrap()[0].options.capture_tool_calls,
        Some(true)
    );
}
#[tokio::test]
async fn tools_record_success_and_failure_then_model_continues() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![calls(&["ok", "bad"]), answer("done")]));
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("ok", Mode::Return)));
    registry.register(Arc::new(Handler::new("bad", Mode::Fail)));
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = builder(model, history.clone(), LoopConfig::default())
        .tools(registry)
        .hooks(hooks.clone())
        .build();
    let (events, result) = collect(agent.start(In::user_text("go")).unwrap()).await;
    assert_eq!(result.unwrap().text, "done");
    let roles: Vec<_> = history.messages().into_iter().map(|m| m.role).collect();
    assert_eq!(
        roles,
        [
            ChatRole::User,
            ChatRole::Assistant,
            ChatRole::Tool,
            ChatRole::Tool,
            ChatRole::Assistant
        ]
    );
    assert!(events
        .iter()
        .any(|e| matches!(e, Out::ToolDone { is_error: true, .. })));
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PostToolUseFailure));
}
#[tokio::test]
async fn hook_changed_arguments_are_validated_and_cannot_bypass_hard_deny() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let security = Arc::new(Security::new(ApprovalDecision::Deny));
    let hooks = Arc::new(Hooks::new(|_, r| {
        if let HookPointOutcome::PreToolUse(o) = &mut r.outcome {
            o.updated_input = Some(json!({"value":2}));
            o.permission = HookPermission::Allow { reason: None };
        }
        if let HookPointOutcome::PermissionDenied(o) = &mut r.outcome {
            o.retry = true;
        }
    }));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("denied")])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .security(security.clone())
    .hooks(hooks.clone())
    .build();
    let result = agent.run(In::user_text("go")).await.unwrap();
    assert_eq!(result.text, "denied");
    assert!(h.inputs.lock().unwrap().is_empty());
    assert_eq!(
        security.seen.lock().unwrap().as_slice(),
        [json!({"value":2}), json!({"value":2})]
    );
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|k| **k == HookEventKind::PermissionDenied)
            .count(),
        2
    );
}
#[tokio::test]
async fn approval_routes_replies_steer_and_follow_up_without_losing_input() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![calls(&["tool"]), answer("done")]));
    let registry = Arc::new(Registry::default());
    let h = Arc::new(Handler::new("tool", Mode::Return));
    registry.register(h.clone());
    let agent = builder(model, history.clone(), LoopConfig::default())
        .tools(registry)
        .security(Arc::new(Security::new(ApprovalDecision::Ask)))
        .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    while let Some(event) = handle.outbox.recv().await {
        if let Out::Ask { id, .. } = event {
            handle.inbox.send(In::follow_up("later")).unwrap();
            handle.inbox.send(In::user_text("steer")).unwrap();
            handle
                .inbox
                .send(In::Reply {
                    id: "wrong".into(),
                    payload: json!({"behavior":"allow"}),
                })
                .unwrap();
            handle
                .inbox
                .send(In::Reply {
                    id,
                    payload: json!({"behavior":"allow","updated_input":{"value":999}}),
                })
                .unwrap();
        }
    }
    let result = handle.join().await.unwrap();
    assert_eq!(result.pending.len(), 1);
    assert!(matches!(&result.pending[0],In::UserText{text,..} if text=="later"));
    assert_eq!(h.inputs.lock().unwrap()[0], json!({"value":1}));
    assert!(history
        .messages()
        .iter()
        .any(|m| m.content.first_text() == Some("steer")));
}
#[tokio::test]
async fn repeated_tool_questions_have_distinct_ids_and_no_deadlock() {
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("tool", Mode::Ask)));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    let mut ids = vec![];
    while let Some(e) = handle.outbox.recv().await {
        if let Out::Ask { id, payload } = e {
            assert_eq!(payload["call_id"], "c0");
            ids.push(id.clone());
            handle
                .inbox
                .send(In::Reply {
                    id,
                    payload: json!({"answer":"yes"}),
                })
                .unwrap();
        }
    }
    assert_eq!(handle.join().await.unwrap().text, "done");
    assert_eq!(ids.len(), 2);
    assert_ne!(ids[0], ids[1]);
}
#[tokio::test]
async fn mcp_hook_answers_without_ui_and_can_replace_tool_output() {
    let history = Arc::new(History::default());
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("mcp__test__ask", Mode::Mcp)));
    let hooks = Arc::new(Hooks::new(|_, r| match &mut r.outcome {
        HookPointOutcome::Elicitation(o) => {
            o.action = Some("accept".into());
            o.content = Some(json!({"choice":"yes"}));
        }
        HookPointOutcome::PostToolUse(o) => {
            o.updated_mcp_tool_output = Some(json!({"redacted":true}))
        }
        _ => {}
    }));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["mcp__test__ask"]), answer("done")])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .hooks(hooks.clone())
    .build();
    assert!(agent.run(In::user_text("go")).await.is_ok());
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::ElicitationResult));
    assert!(serde_json::to_string(&history.messages())
        .unwrap()
        .contains("redacted"));
}
#[tokio::test]
async fn compaction_and_retry_are_bounded_and_do_not_repeat_user_history() {
    let history = Arc::new(History::default());
    history.tokens.store(100, Ordering::SeqCst);
    let mut model = Model::new(vec![error_stream(), answer("done")]);
    model.recovery = ModelRecovery::Compact;
    let model = Arc::new(model);
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let config = LoopConfig {
        ..Default::default()
    };
    let agent = builder(model.clone(), history.clone(), config)
        .hooks(hooks.clone())
        .build();
    let result = agent.run(In::user_text("go")).await.unwrap();
    assert_eq!(result.usage.unwrap().total_tokens, 11);
    assert_eq!(history.compactions.lock().unwrap().len(), 2);
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .count(),
        1
    );
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|k| **k == HookEventKind::PostCompact)
            .count(),
        0
    );
}
#[tokio::test]
async fn stop_can_continue_but_not_forever() {
    let count = Arc::new(AtomicUsize::new(0));
    let c = count.clone();
    let hooks = Arc::new(Hooks::new(move |i, r| {
        if matches!(i.event, HookEvent::Stop { .. }) {
            c.fetch_add(1, Ordering::SeqCst);
            block(r);
        }
    }));
    let model = Arc::new(Model::new(vec![answer("first"), answer("second")]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig {
            max_stop_continuations: 1,
            ..Default::default()
        },
    )
    .hooks(hooks)
    .build();
    let error = agent.run(In::user_text("go")).await.unwrap_err();
    assert_eq!(error.output.text, "second");
    assert_eq!(count.load(Ordering::SeqCst), 2);
}
#[tokio::test]
async fn cancel_preserves_partial_answer_and_pending_inputs() {
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![hangs_after("partial")])),
        history.clone(),
        LoopConfig::default(),
    )
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    while let Some(e) = handle.outbox.recv().await {
        if matches!(e, Out::Chunk { .. }) {
            handle.inbox.send(In::follow_up("later")).unwrap();
            handle.cancel.cancel();
        }
    }
    let error = handle.join().await.unwrap_err();
    assert!(matches!(
        *error.error,
        YourAiError::Aborted(AbortReason::Cancelled)
    ));
    assert_eq!(error.output.text, "partial");
    assert_eq!(error.output.pending.len(), 1);
    assert_eq!(
        history.messages().last().unwrap().content.first_text(),
        Some("partial")
    );
}
#[tokio::test]
async fn tool_cancel_completes_unresolved_batch_without_running_remaining_tools() {
    let registry = Arc::new(Registry::default());
    let first = Arc::new(Handler::new("slow", Mode::Hang));
    let second = Arc::new(Handler::new("later", Mode::Return));
    registry.register(first);
    registry.register(second.clone());
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["slow", "later"])])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    let mut done = 0;
    while let Some(e) = handle.outbox.recv().await {
        match e {
            Out::ToolProgress { .. } => handle.cancel.cancel(),
            Out::ToolDone { .. } => done += 1,
            _ => {}
        }
    }
    assert!(handle.join().await.is_err());
    assert_eq!(done, 2);
    assert!(second.inputs.lock().unwrap().is_empty());
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count(),
        2
    );
}
#[tokio::test]
async fn disconnected_output_interrupts_a_silent_model_wait() {
    let agent = builder(
        Arc::new(Model::new(vec![Box::pin(futures_util::stream::pending())])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    handle.outbox.close();
    let error = tokio::time::timeout(Duration::from_secs(1), handle.join())
        .await
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        *error.error,
        YourAiError::Aborted(AbortReason::Disconnected)
    ));
}
#[tokio::test]
async fn visible_model_failure_is_not_retried_and_fires_stop_failure() {
    use futures_util::StreamExt;
    let stream =
        Box::pin(futures_util::stream::iter(vec![Ok(chunk("partial"))]).chain(error_stream()));
    let mut m = Model::new(vec![stream, answer("never")]);
    m.recovery = ModelRecovery::Retry;
    let model = Arc::new(m);
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .hooks(hooks.clone())
    .build();
    assert_eq!(
        agent
            .run(In::user_text("go"))
            .await
            .unwrap_err()
            .output
            .text,
        "partial"
    );
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::StopFailure));
}
#[tokio::test]
async fn deadlines_and_zero_model_budget_stop_before_side_effects() {
    for deadline in [false, true] {
        let model = Arc::new(Model::new(vec![answer("never")]));
        let agent = builder(
            model.clone(),
            Arc::new(History::default()),
            LoopConfig::default(),
        )
        .build();
        let mut options = TurnOptions::default();
        if deadline {
            options.limits.deadline = Some(Instant::now());
        } else {
            options.limits.max_model_calls = Some(0);
        }
        let error = agent
            .run_with(In::user_text("go"), options)
            .await
            .unwrap_err();
        assert!(matches!(*error.error, YourAiError::Aborted(_)));
        assert!(model.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn rejected_prompt_never_enters_history_or_model() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![]));
    let hooks = Arc::new(Hooks::new(|i, r| {
        if matches!(i.event, HookEvent::UserPromptSubmit { .. }) {
            block(r);
        }
    }));
    let agent = builder(model.clone(), history.clone(), LoopConfig::default())
        .hooks(hooks)
        .build();
    assert!(agent.run(In::user_text("blocked")).await.is_ok());
    assert!(history.messages().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
}
#[tokio::test]
async fn invisible_failure_retries_only_up_to_policy_limit() {
    let mut m = Model::new(vec![error_stream(), error_stream(), answer("never")]);
    m.recovery = ModelRecovery::Retry;
    let model = Arc::new(m);
    let config = LoopConfig {
        max_model_retries: 1,
        retry_delay: Duration::ZERO,
        ..Default::default()
    };
    let agent = builder(model.clone(), Arc::new(History::default()), config).build();
    assert!(agent.run(In::user_text("go")).await.is_err());
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}
#[tokio::test]
async fn invalid_hook_arguments_and_truncated_calls_never_execute() {
    for truncated in [false, true] {
        let history = Arc::new(History::default());
        let h = Arc::new(Handler::new("tool", Mode::Return));
        let registry = Arc::new(Registry::default());
        registry.register(h.clone());
        let mut terminal = end("partial", vec![call("c0", "tool")]);
        if truncated {
            if let ChatStreamEvent::End(e) = &mut terminal {
                e.captured_stop_reason = Some(StopReason::MaxTokens("length".into()));
            }
        }
        let model = Arc::new(Model::new(vec![
            events(vec![terminal]),
            answer("bad input"),
        ]));
        let hooks = Arc::new(Hooks::new(|_, r| {
            if let HookPointOutcome::PreToolUse(o) = &mut r.outcome {
                o.updated_input = Some(json!({"value":"invalid"}));
            }
        }));
        let agent = builder(model, history.clone(), LoopConfig::default())
            .tools(registry)
            .hooks(hooks)
            .build();
        let result = agent.run(In::user_text("go")).await;
        assert_eq!(result.is_err(), truncated);
        assert!(h.inputs.lock().unwrap().is_empty());
        if truncated {
            assert_eq!(result.unwrap_err().output.text, "partial");
            assert_eq!(
                history.messages().last().unwrap().content.first_text(),
                Some("partial")
            );
        }
    }
}
#[tokio::test]
async fn tool_budget_ends_turn_and_completes_remaining_call_records() {
    let history = Arc::new(History::default());
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool", "tool"])])),
        history.clone(),
        LoopConfig {
            max_tool_calls: 1,
            ..Default::default()
        },
    )
    .tools(registry)
    .build();
    let failure = agent.run(In::user_text("go")).await.unwrap_err();
    assert!(matches!(
        *failure.error,
        YourAiError::Aborted(AbortReason::LimitReached(TurnLimit::ToolCalls))
    ));
    assert_eq!(h.inputs.lock().unwrap().len(), 1);
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count(),
        2
    );
}
#[tokio::test(start_paused = true)]
async fn tool_timeout_becomes_tool_result_and_continues() {
    let h = Arc::new(Handler::new("tool", Mode::Hang));
    let registry = Arc::new(Registry::default());
    registry.register(h);
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("timed out")])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut options = TurnOptions::default();
    options.limits.tool_timeout = Some(Duration::from_millis(5));
    let (events, result) = collect(agent.start_with(In::user_text("go"), options).unwrap()).await;
    assert_eq!(result.unwrap().text, "timed out");
    assert!(events
        .iter()
        .any(|e| matches!(e, Out::ToolDone { is_error: true, .. })));
}
#[tokio::test(start_paused = true)]
async fn approval_timeout_is_denial_not_permission() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("denied")])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .hooks(hooks.clone())
    .security(Arc::new(Security::new(ApprovalDecision::Ask)))
    .build();
    let mut options = TurnOptions::default();
    options.limits.approval_timeout = Some(Duration::from_millis(5));
    let (_, result) = collect(agent.start_with(In::user_text("go"), options).unwrap()).await;
    assert!(result.is_ok());
    assert!(h.inputs.lock().unwrap().is_empty());
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PermissionDenied));
}
#[tokio::test]
async fn noninteractive_tool_question_returns_config_without_hanging() {
    let h = Arc::new(Handler::new("tool", Mode::Ask));
    let registry = Arc::new(Registry::default());
    registry.register(h);
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"])])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let failure = agent.run(In::user_text("go")).await.unwrap_err();
    assert!(matches!(
        *failure.error,
        YourAiError::Error(ErrorKind::Config(_))
    ));
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
async fn real_hook_runtime_registration_effect_is_consumed_by_loop() {
    struct Native;
    impl HookHandler for Native {
        fn execute<'a>(
            &'a self,
            _: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
            Box::pin(async {
                Ok(HookOutput::Parsed(
                    json!({"hookSpecificOutput":{"hookEventName":"UserPromptSubmit","additionalContext":"real runtime context"}}),
                ))
            })
        }
    }
    let runtime = Arc::new(yourai_hooks::runtime::ConcreteHookRuntime::new());
    runtime
        .register(NativeHookRegistration {
            id: "context".into(),
            event: HookEventKind::UserPromptSubmit,
            matcher: None,
            handler: Arc::new(Native),
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        })
        .await
        .unwrap();
    let model = Arc::new(Model::new(vec![answer("done")]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .hooks(runtime)
    .build();
    agent.run(In::user_text("go")).await.unwrap();
    assert!(model.requests.lock().unwrap()[0]
        .request
        .messages
        .iter()
        .any(|m| m
            .content
            .first_text()
            .is_some_and(|t| t.contains("real runtime context"))));
}
#[tokio::test]
async fn post_hook_stop_retains_actual_side_effect_result() {
    let history = Arc::new(History::default());
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let hooks = Arc::new(Hooks::new(|i, r| {
        if matches!(i.event, HookEvent::PostToolUse { .. }) {
            r.common.prevent_continuation = true;
        }
    }));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool", "tool"])])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .hooks(hooks)
    .build();
    let (events, result) = collect(agent.start(In::user_text("go")).unwrap()).await;
    assert!(result.is_err());
    assert_eq!(h.inputs.lock().unwrap().len(), 1);
    assert!(events.iter().any(|e|matches!(e,Out::ToolDone{id,output,is_error:false,..} if id=="c0" && *output==json!({"value":1}))));
    assert_eq!(
        history
            .messages()
            .iter()
            .filter(|m| m.role == ChatRole::Tool)
            .count(),
        2
    );
}

#[tokio::test]
async fn repeated_call_identity_never_replays_a_completed_tool() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), calls(&["tool"])])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    assert!(agent.run(In::user_text("go")).await.is_err());
    assert_eq!(h.inputs.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn permission_hook_modified_scope_is_rechecked_against_hard_policy() {
    struct Policy;
    impl SecurityProvider for Policy {
        fn check_tool_call<'a>(
            &'a self,
            c: &'a SecurityContext,
        ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
            Box::pin(async move {
                Ok(if c.input["value"] == 2 {
                    ApprovalDecision::Deny
                } else {
                    ApprovalDecision::Ask
                })
            })
        }
        fn check_command<'a>(
            &'a self,
            _: &'a str,
        ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
            Box::pin(async { Ok(PolicyDecision::Deny) })
        }
        fn check_file_access<'a>(
            &'a self,
            _: &'a str,
            _: bool,
        ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
            Box::pin(async { Ok(PolicyDecision::Deny) })
        }
    }
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let hooks = Arc::new(Hooks::new(|i, r| {
        if let HookPointOutcome::PermissionRequest(o) = &mut r.outcome {
            o.decision = Some(PermissionRequestDecision {
                behavior: PermissionRequestBehavior::Allow,
                updated_input: Some(json!({"value":2})),
                updated_permissions: vec![],
                message: None,
                interrupt: false,
            });
        }
        if let HookEvent::PermissionDenied { tool_input, .. } = &i.event {
            assert_eq!(*tool_input, json!({"value":2}));
        }
    }));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("denied")])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .hooks(hooks)
    .security(Arc::new(Policy))
    .build();
    assert!(agent.run(In::user_text("go")).await.is_ok());
    assert!(h.inputs.lock().unwrap().is_empty());
}
