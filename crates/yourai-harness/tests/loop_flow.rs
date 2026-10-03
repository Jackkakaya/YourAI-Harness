#[path = "support/loop.rs"]
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
use yourai_harness::default_loop::LoopConfig;

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
async fn image_attachment_is_committed_as_binary_and_reaches_the_model() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![answer("ok")]));
    let agent = builder(model.clone(), history.clone(), LoopConfig::default()).build();
    // attachment resolution decodes and normalizes image attachments, so the
    // payload must be a real image.
    let data = tiny_png_base64();
    let (_events, result) = collect(
        agent
            .start(In::user_text_with_attachments(
                "describe this",
                vec![UserAttachment::base64(
                    "image/png",
                    data,
                    Some("clip.png".into()),
                )],
            ))
            .unwrap(),
    )
    .await;
    result.unwrap();

    // The committed user message carries the text plus one Binary image part.
    let user = &history.messages()[0];
    assert_eq!(user.role, ChatRole::User);
    assert_eq!(user.content.texts(), vec!["describe this"]);
    let binaries = user.content.binaries();
    assert_eq!(binaries.len(), 1);
    assert_eq!(binaries[0].content_type, "image/png");
    assert_eq!(binaries[0].name.as_deref(), Some("clip.png"));
    assert!(binaries[0].is_image());

    // The image reaches the model request unchanged (projection keeps User
    // Binary parts when limit_tools is true, as build_request does).
    let req = &model.requests.lock().unwrap()[0];
    let req_user = req
        .request
        .messages
        .iter()
        .find(|m| m.role == ChatRole::User)
        .unwrap();
    assert_eq!(req_user.content.binaries().len(), 1);
}

#[tokio::test]
async fn file_reference_attachment_becomes_bounded_text_in_history() {
    let history = Arc::new(History::default());
    let model = Arc::new(Model::new(vec![answer("reviewed")]));
    let agent = builder(model, history.clone(), LoopConfig::default()).build();

    // A real file on disk, referenced (not inlined) by the frontend.
    let dir = std::env::temp_dir().join("yourai_loop_file_ref");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    let file = dir.join("notes.rs");
    std::fs::write(&file, "fn one() {}\nfn two() {}\nfn three() {}\n").unwrap();

    let (_events, result) = collect(
        agent
            .start(In::user_text_with_attachments(
                "review this",
                vec![UserAttachment::file(
                    file.to_string_lossy().into_owned(),
                    Some((2, 3)),
                )],
            ))
            .unwrap(),
    )
    .await;
    result.unwrap();
    let _ = std::fs::remove_dir_all(&dir);

    // The referenced file is read harness-side and committed as a text part
    // (provenance note + line window + fence), never as a Binary part.
    let user = &history.messages()[0];
    assert_eq!(user.role, ChatRole::User);
    assert!(user.content.binaries().is_empty());
    let text = user.content.texts().join("\n");
    assert!(text.contains("review this"), "{text}");
    assert!(text.contains("[Attached file"), "{text}");
    assert!(text.contains("lines 2-3"), "{text}");
    assert!(text.contains("fn two() {}"), "{text}");
    assert!(text.contains("fn three() {}"), "{text}");
    assert!(!text.contains("fn one()"), "{text}");
}

/// Real 8x8 PNG, base64-encoded — the loop decodes image attachments for
/// normalization, so fabricated payloads are rejected.
fn tiny_png_base64() -> String {
    let img = image::DynamicImage::new_rgb8(8, 8);
    let mut buf = Vec::new();
    img.to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(&buf)
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
        2 // Replacement ContextManager summaries use the common hook wrapper too.
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
async fn slow_but_progressing_stream_completes_beyond_total_budget() {
    use futures_util::StreamExt;
    let gap = Duration::from_millis(100);
    let stream = futures_util::stream::iter(vec![chunk("a"), chunk("b"), end("ab", vec![])]).then(
        move |event| async move {
            tokio::time::sleep(gap).await;
            Ok(event)
        },
    );
    let agent = builder(
        Arc::new(Model::new(vec![Box::pin(stream)])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .build();
    assert_eq!(
        agent
            .run_with(
                In::user_text("go"),
                model_timeout(Duration::from_millis(200))
            )
            .await
            .unwrap()
            .text,
        "ab"
    );
}
#[tokio::test]
async fn model_call_limit_forces_final_text_only_summary() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    let model = Arc::new(Model::new(vec![calls(&["tool"]), answer("final summary")]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut options = TurnOptions::default();
    options.limits.steps = Some(2);
    let result = agent.run_with(In::user_text("go"), options).await.unwrap();
    assert_eq!(result.text, "final summary");
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    assert!(!requests[0]
        .request
        .tools
        .as_deref()
        .unwrap_or(&[])
        .is_empty());
    assert!(requests[1]
        .request
        .tools
        .as_deref()
        .unwrap_or(&[])
        .is_empty());
    assert_eq!(h.inputs.lock().unwrap().len(), 1);
    // OpenCode runner alignment: the final request carries an assistant-role
    // MAX_STEPS_PROMPT prefill (request-only, never persisted) and forbids
    // tool calls at the API level.
    let last = requests[1].request.messages.last().unwrap();
    assert_eq!(last.role, ChatRole::Assistant);
    assert!(last
        .content
        .first_text()
        .is_some_and(|t| t.contains("MAXIMUM STEPS REACHED")));
    assert_eq!(requests[1].options.tool_choice, Some(ToolChoice::None));
}
#[tokio::test]
async fn model_call_limit_never_executes_calls_from_the_final_step() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    // The final step still returns a tool call; opencode failUnsettledTools
    // semantics feed an explicit failure result back so the model can produce
    // the required text-only summary on the next iteration.
    let model = Arc::new(Model::new(vec![calls(&["tool"]), answer("wrapped up")]));
    let history = Arc::new(History::default());
    let agent = builder(model.clone(), history.clone(), LoopConfig::default())
        .tools(registry)
        .build();
    let mut options = TurnOptions::default();
    options.limits.steps = Some(1);
    let result = agent.run_with(In::user_text("go"), options).await.unwrap();
    assert!(h.inputs.lock().unwrap().is_empty());
    assert_eq!(result.text, "wrapped up");
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert!(request.request.tools.as_deref().unwrap_or(&[]).is_empty());
        assert_eq!(request.options.tool_choice, Some(ToolChoice::None));
    }
    // The refused call is persisted as an error tool result so history stays
    // complete for the next turn.
    let tool_message = history
        .messages()
        .into_iter()
        .find(|m| m.role == ChatRole::Tool)
        .expect("refused tool call must leave a tool-result record");
    let response = tool_message
        .content
        .parts()
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolResponse(response) => Some(response),
            _ => None,
        })
        .unwrap();
    assert!(response
        .content
        .contains("Tools are disabled after the maximum agent steps"));
}
#[tokio::test]
async fn forced_final_bound_notifies_and_completes_instead_of_looping() {
    let h = Arc::new(Handler::new("tool", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h.clone());
    // A misbehaving provider keeps requesting tools after the step cap:
    // every refusal warns the user, and once the defensive bound is
    // exhausted the turn completes with the text it has instead of asking
    // the model again (Stop hooks still run through complete). Call ids
    // must differ per iteration: identities already seen this turn are
    // refused as duplicates.
    let refused = |id: &str| events(vec![end("", vec![call(id, "tool")])]);
    let model = Arc::new(Model::new(vec![
        refused("c0"),
        refused("c1"),
        refused("c2"),
        refused("c3"),
    ]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut options = TurnOptions::default();
    options.limits.steps = Some(1);
    let (events, result) = collect(agent.start_with(In::user_text("go"), options).unwrap()).await;
    result.expect("bounded forced-final turn completes");
    assert!(
        h.inputs.lock().unwrap().is_empty(),
        "refused calls never execute"
    );
    let notices: Vec<&str> = events
        .iter()
        .filter_map(|e| match e {
            Out::Notice {
                level: Level::Warning,
                message,
            } => Some(message.as_str()),
            _ => None,
        })
        .collect();
    // One batch warning per refused iteration (the bound iteration included),
    // plus the one-time bound-exhausted explanation.
    assert_eq!(
        notices
            .iter()
            .filter(|m| **m == "Tools are disabled after the maximum agent steps")
            .count(),
        4
    );
    assert_eq!(
        notices
            .iter()
            .filter(|m| **m
                == "Model requested tools after maximum agent steps; they were not executed.")
            .count(),
        1
    );
    // The bound stops the model polling: initial call plus three continuations.
    assert_eq!(model.requests.lock().unwrap().len(), 4);
    // Every request after the cap disables tools at the API level.
    for request in model.requests.lock().unwrap().iter() {
        assert_eq!(request.options.tool_choice, Some(ToolChoice::None));
    }
}
#[tokio::test]
async fn invisible_failure_announces_structured_retry_status() {
    let mut m = Model::new(vec![error_stream(), answer("resumed")]);
    m.recovery = ModelRecovery::Retry;
    let agent = builder(
        Arc::new(m),
        Arc::new(History::default()),
        LoopConfig {
            retry_delay: Duration::ZERO,
            ..Default::default()
        },
    )
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    let mut seen = vec![];
    while let Some(e) = handle.outbox.recv().await {
        if let Out::Retry {
            attempt,
            max,
            wait_ms,
            ..
        } = e
        {
            seen.push((attempt, max, wait_ms));
        }
    }
    assert_eq!(seen, vec![(1, 5, 0)]);
    assert_eq!(handle.join().await.unwrap().text, "resumed");
}
#[tokio::test]
async fn silent_stream_still_fails_on_per_event_timeout() {
    let agent = builder(
        Arc::new(Model::new(vec![Box::pin(futures_util::stream::pending())])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .build();
    let error = agent
        .run_with(
            In::user_text("go"),
            model_timeout(Duration::from_millis(50)),
        )
        .await
        .unwrap_err();
    assert!(matches!(
        *error.error,
        YourAiError::Error(ErrorKind::Provider { name: "model", .. })
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
async fn deadlines_and_zero_steps_stop_before_side_effects() {
    let model = Arc::new(Model::new(vec![answer("never")]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .build();
    let mut options = TurnOptions::default();
    options.limits.deadline = Some(Instant::now());
    let error = agent
        .run_with(In::user_text("go"), options)
        .await
        .unwrap_err();
    assert!(matches!(
        *error.error,
        YourAiError::Aborted(AbortReason::DeadlineExceeded)
    ));
    assert!(model.requests.lock().unwrap().is_empty());

    let model = Arc::new(Model::new(vec![answer("never")]));
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .build();
    let mut options = TurnOptions::default();
    options.limits.steps = Some(0);
    let error = agent
        .run_with(In::user_text("go"), options)
        .await
        .unwrap_err();
    assert!(matches!(
        *error.error,
        YourAiError::Error(ErrorKind::Config(_))
    ));
    assert!(model.requests.lock().unwrap().is_empty());
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
    let output = agent.run(In::user_text("blocked")).await.unwrap();
    assert!(output.pending.is_empty());
    assert_eq!(output.rejected.len(), 1);
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
    let runtime = Arc::new(yourai_harness::hooks::runtime::ConcreteHookRuntime::new());
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

#[tokio::test]
async fn retry_respects_max_retries_limit() {
    let mut model = Model::new(vec![error_stream(), answer("must not run")]);
    model.recovery = ModelRecovery::Retry;
    let model = Arc::new(model);
    let agent = builder(
        model.clone(),
        Arc::new(History::default()),
        LoopConfig {
            retry_delay: Duration::ZERO,
            max_model_retries: 0,
            ..Default::default()
        },
    )
    .build();
    let err = agent.run(In::user_text("go")).await.unwrap_err();
    assert!(matches!(
        *err.error,
        YourAiError::Error(ErrorKind::Provider { name: "model", .. })
    ));
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn compaction_does_not_consume_step() {
    let history = Arc::new(History::default());
    history.tokens.store(100, Ordering::SeqCst);
    let model = Arc::new(Model::new(vec![answer("ok")]));
    let agent = builder(model.clone(), history.clone(), LoopConfig::default()).build();
    let mut options = TurnOptions::default();
    options.limits.steps = Some(1);
    let result = agent.run_with(In::user_text("go"), options).await.unwrap();
    assert_eq!(result.text, "ok");
    assert_eq!(history.compactions.lock().unwrap().len(), 1);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn invalid_attachments_are_returned_without_history_or_retry() {
    let dir = tempfile::tempdir().unwrap();
    let unsupported = dir.path().join("data.zip");
    std::fs::write(&unsupported, b"zip").unwrap();
    let gone = dir.path().join("deleted.rs");
    std::fs::write(&gone, "text").unwrap();
    let missing_attachment = UserAttachment::file(gone.to_string_lossy(), None);
    std::fs::remove_file(&gone).unwrap();
    for bad in [
        UserAttachment::file(unsupported.to_string_lossy(), None),
        missing_attachment,
        UserAttachment::base64("image/png", "not base64", None),
    ] {
        let history = Arc::new(History::default());
        let model = Arc::new(Model::new(vec![]));
        let agent = builder(model.clone(), history.clone(), LoopConfig::default()).build();
        let input = In::user_text_with_attachments("repair me", vec![bad]);
        let original = serde_json::to_value(&input).unwrap();
        let (events, result) = collect(agent.start(input).unwrap()).await;
        let output = result.unwrap();
        assert!(output.pending.is_empty());
        assert_eq!(output.rejected.len(), 1);
        assert_eq!(
            serde_json::to_value(&output.rejected[0].input).unwrap(),
            original
        );
        let rejections: Vec<_> = events
            .iter()
            .filter_map(|e| match e {
                Out::InputRejected { rejection } => Some(rejection),
                _ => None,
            })
            .collect();
        assert_eq!(rejections.len(), 1);
        assert!(!rejections[0].reason.is_empty());
        assert_eq!(
            serde_json::to_value(&rejections[0].input).unwrap(),
            original
        );
        assert!(history.messages().is_empty());
        assert!(model.requests.lock().unwrap().is_empty());
    }
}

#[tokio::test]
async fn non_streaming_callers_receive_rejected_input_too() {
    let history = Arc::new(History::default());
    let agent = builder(Arc::new(Model::new(vec![])), history, LoopConfig::default()).build();
    let output = agent
        .run(In::user_text_with_attachments(
            "bad",
            vec![UserAttachment::base64("image/png", "bad", None)],
        ))
        .await
        .unwrap();
    assert_eq!(output.rejected.len(), 1);
    assert!(output.pending.is_empty());
}

#[tokio::test]
async fn temporary_history_failure_preserves_input_for_retry() {
    let history = Arc::new(History::default());
    history.append_failures.store(1, Ordering::SeqCst);
    let model = Arc::new(Model::new(vec![answer("done")]));
    let agent = builder(model.clone(), history.clone(), LoopConfig::default()).build();
    let input = In::user_text_with_attachments(
        "valid",
        vec![UserAttachment::base64("image/png", tiny_png_base64(), None)],
    );
    let (events, result) = collect(agent.start(input).unwrap()).await;
    let mut failed = result.unwrap_err();
    assert_eq!(failed.output.pending.len(), 1);
    assert!(failed.output.rejected.is_empty());
    assert!(!events
        .iter()
        .any(|e| matches!(e, Out::InputRejected { .. })));
    assert!(history.messages().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    let (_, result) = collect(agent.start(failed.output.pending.remove(0)).unwrap()).await;
    let output = result.unwrap();
    assert_eq!(output.text, "done");
    assert!(output.pending.is_empty());
    assert!(output.rejected.is_empty());
    assert_eq!(history.messages().len(), 2);
}

#[tokio::test]
async fn relative_file_references_resolve_against_the_session_directory() {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(dir.path().join("session-only.rs"), "session-local content").unwrap();
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![answer("done")])),
        history.clone(),
        LoopConfig::default(),
    )
    .build();
    let mut options = TurnOptions::default();
    options.session = Some(Arc::new(SessionContext::new(
        history.id.clone(),
        dir.path(),
    )));
    let (_, output) = collect(
        agent
            .start_with(
                In::user_text_with_attachments(
                    "read",
                    vec![UserAttachment::file("session-only.rs", None)],
                ),
                options,
            )
            .unwrap(),
    )
    .await;
    assert!(output.unwrap().rejected.is_empty());
    assert!(history.messages()[0]
        .content
        .texts()
        .join("\n")
        .contains("session-local content"));
}

#[tokio::test]
async fn invalid_steer_does_not_block_valid_steer_or_follow_up() {
    let history = Arc::new(History::default());
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("tool", Mode::Return)));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .security(Arc::new(Security::new(ApprovalDecision::Ask)))
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    let mut rejected = 0;
    while let Some(event) = handle.outbox.recv().await {
        match event {
            Out::Ask { id, .. } => {
                handle
                    .inbox
                    .send(In::user_text_with_attachments(
                        "invalid",
                        vec![UserAttachment::base64("image/png", "bad", None)],
                    ))
                    .unwrap();
                handle.inbox.send(In::user_text("valid steer")).unwrap();
                handle.inbox.send(In::follow_up("later")).unwrap();
                handle
                    .inbox
                    .send(In::Reply {
                        id,
                        payload: json!({"behavior":"allow"}),
                    })
                    .unwrap();
            }
            Out::InputRejected { .. } => rejected += 1,
            _ => {}
        }
    }
    let output = handle.join().await.unwrap();
    assert_eq!(output.text, "done");
    assert_eq!(rejected, 1);
    assert_eq!(output.rejected.len(), 1);
    assert_eq!(output.pending.len(), 1);
    assert!(matches!(&output.pending[0], In::UserText { text, .. } if text == "later"));
    assert!(history
        .messages()
        .iter()
        .any(|m| m.content.first_text() == Some("valid steer")));
    assert!(!history
        .messages()
        .iter()
        .any(|m| m.content.first_text() == Some("invalid")));
}

#[tokio::test(start_paused = true)]
async fn default_approval_and_tool_questions_survive_long_user_waits() {
    for permission in [true, false] {
        let handler = Arc::new(Handler::new(
            "tool",
            if permission { Mode::Return } else { Mode::Ask },
        ));
        let registry = Arc::new(Registry::default());
        registry.register(handler.clone());
        let mut b = builder(
            Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])),
            Arc::new(History::default()),
            LoopConfig::default(),
        )
        .tools(registry);
        if permission {
            b = b.security(Arc::new(Security::new(ApprovalDecision::Ask)));
        }
        let agent = b.build();
        let mut handle = agent
            .start_with(
                In::user_text("go"),
                model_timeout(Duration::from_secs(86_400)),
            )
            .unwrap();
        let mut asks = 0;
        while let Some(event) = handle.outbox.recv().await {
            if let Out::Ask { id, .. } = event {
                asks += 1;
                // Exceed both the old 300s approval and 610s outer tool timeout.
                tokio::time::advance(Duration::from_secs(900)).await;
                tokio::task::yield_now().await;
                handle
                    .inbox
                    .send(In::Reply {
                        id,
                        payload: if permission {
                            json!({"behavior":"allow"})
                        } else {
                            json!({"answer":"yes"})
                        },
                    })
                    .unwrap();
            }
        }
        assert_eq!(handle.join().await.unwrap().text, "done");
        assert_eq!(asks, if permission { 1 } else { 2 });
        assert_eq!(handler.inputs.lock().unwrap().len(), 1);
    }
}

#[tokio::test(start_paused = true)]
async fn cancelled_tool_bounds_failure_hook_then_records_interruption() {
    struct StuckFailureHook;
    impl HookRuntime for StuckFailureHook {
        fn dispatch<'a>(
            &'a self,
            i: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
            Box::pin(async move {
                if matches!(i.event, HookEvent::PostToolUseFailure { .. }) {
                    std::future::pending::<()>().await;
                }
                Ok(HookDispatchResult::empty(i.event.kind()))
            })
        }
    }
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("tool", Mode::Hang)));
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"])])),
        history.clone(),
        LoopConfig::default(),
    )
    .tools(registry)
    .hooks(Arc::new(StuckFailureHook))
    .build();
    let mut handle = agent.start(In::user_text("go")).unwrap();
    while let Some(event) = handle.outbox.recv().await {
        if matches!(event, Out::ToolProgress { .. }) {
            break;
        }
    }
    let start = tokio::time::Instant::now();
    handle.interrupt();
    let (events, result) = tokio::time::timeout(Duration::from_secs(1), collect(handle))
        .await
        .unwrap();
    assert!(matches!(
        *result.unwrap_err().error,
        YourAiError::Aborted(AbortReason::Cancelled)
    ));
    assert!(tokio::time::Instant::now() - start <= Duration::from_millis(251));
    assert!(events
        .iter()
        .any(|e| matches!(e, Out::ToolDone { is_error: true, .. })));
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
async fn explicit_deadline_still_cancels_an_unlimited_question() {
    let registry = Arc::new(Registry::default());
    registry.register(Arc::new(Handler::new("tool", Mode::Ask)));
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["tool"])])),
        Arc::new(History::default()),
        LoopConfig::default(),
    )
    .tools(registry)
    .build();
    let mut options = TurnOptions::default();
    options.limits.deadline = Some(Instant::now() + Duration::from_millis(50));
    let mut handle = agent.start_with(In::user_text("go"), options).unwrap();
    while let Some(event) = handle.outbox.recv().await {
        if matches!(event, Out::Ask { .. }) {
            break;
        }
    }
    let (_, result) = tokio::time::timeout(Duration::from_secs(1), collect(handle))
        .await
        .unwrap();
    assert!(matches!(
        *result.unwrap_err().error,
        YourAiError::Aborted(AbortReason::DeadlineExceeded)
    ));
}

#[tokio::test(start_paused = true)]
async fn cancelled_or_timed_out_tool_preserves_a_completion_within_grace() {
    struct SettlingTool;
    impl ToolHandler for SettlingTool {
        fn name(&self) -> &str {
            "tool"
        }
        fn definition(&self) -> Tool {
            Tool::new("tool")
        }
        fn security_context(&self, input: &serde_json::Value) -> SecurityContext {
            SecurityContext {
                action: "tool".into(),
                input: input.clone(),
                is_destructive: false,
                is_network: false,
            }
        }
        fn run<'a>(
            &'a self,
            tc: ToolContext<'a>,
            _: serde_json::Value,
        ) -> BoxFuture<'a, Result<serde_json::Value, YourAiError>> {
            Box::pin(async move {
                tc.emit_progress(json!("started"));
                tc.cancel.cancelled().await;
                tokio::time::sleep(Duration::from_millis(100)).await;
                Ok(json!({"completed":true}))
            })
        }
    }
    for timeout in [false, true] {
        let registry = Arc::new(Registry::default());
        registry.register(Arc::new(SettlingTool));
        let history = Arc::new(History::default());
        let agent = builder(
            Arc::new(Model::new(vec![calls(&["tool"]), answer("done")])),
            history.clone(),
            LoopConfig::default(),
        )
        .tools(registry)
        .build();
        let mut options = TurnOptions::default();
        if timeout {
            options.limits.tool_timeout = Some(Duration::from_millis(5));
        }
        let mut handle = agent.start_with(In::user_text("go"), options).unwrap();
        while let Some(event) = handle.outbox.recv().await {
            if matches!(event, Out::ToolProgress { .. }) {
                break;
            }
        }
        if !timeout {
            handle.interrupt();
        }
        let (events, result) = tokio::time::timeout(Duration::from_secs(1), collect(handle))
            .await
            .unwrap();
        if timeout {
            assert_eq!(result.unwrap().text, "done");
        } else {
            assert!(matches!(
                *result.unwrap_err().error,
                YourAiError::Aborted(AbortReason::Cancelled)
            ));
        }
        assert!(events.iter().any(|e| matches!(e, Out::ToolDone { output, is_error: false, .. } if output["completed"] == true)));
        let messages = history.messages();
        let results: Vec<_> = messages
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].content.contains("completed"));
    }
}

fn model_timeout(timeout: Duration) -> TurnOptions {
    let mut options = TurnOptions::default();
    options.limits.model_timeout = Some(timeout);
    options
}

#[tokio::test]
async fn repeated_calls_use_existing_permission_ui_and_honor_denial() {
    for allow in [true, false] {
        let h = Arc::new(Handler::new("tool", Mode::Return));
        let registry = Arc::new(Registry::default());
        registry.register(h.clone());
        let mut streams: Vec<_> = (0..4)
            .map(|i| events(vec![end("", vec![call(&format!("repeat-{i}"), "tool")])]))
            .collect();
        streams.push(answer("done"));
        let agent = builder(
            Arc::new(Model::new(streams)),
            Arc::new(History::default()),
            LoopConfig::default(),
        )
        .tools(registry)
        .build();
        let mut handle = agent.start(In::user_text("go")).unwrap();
        let mut asks = 0;
        while let Some(event) = handle.outbox.recv().await {
            if let Out::Ask { id, payload } = event {
                asks += 1;
                assert_eq!(payload["kind"], "permission");
                assert!(payload["reason"].as_str().unwrap().contains("doom_loop"));
                assert_eq!(payload["tool_name"], "tool");
                handle
                    .inbox
                    .send(In::Reply {
                        id,
                        payload: json!({"behavior":if allow {"allow"} else {"deny"}}),
                    })
                    .unwrap();
            }
        }
        assert_eq!(handle.join().await.unwrap().text, "done");
        assert_eq!(asks, 2);
        assert_eq!(h.inputs.lock().unwrap().len(), if allow { 4 } else { 2 });
    }
}

#[tokio::test]
async fn repeated_calls_allow_hook_rewrites_and_yolo_without_extra_gate() {
    for yolo in [false, true] {
        let h = Arc::new(Handler::new("tool", Mode::Return));
        let registry = Arc::new(Registry::default());
        registry.register(h.clone());
        let hooks = Arc::new(Hooks::new(|_, r| {
            if let HookPointOutcome::PermissionRequest(o) = &mut r.outcome {
                o.decision = Some(PermissionRequestDecision {
                    behavior: PermissionRequestBehavior::Allow,
                    updated_input: Some(json!({"value":2})),
                    updated_permissions: vec![],
                    message: None,
                    interrupt: false,
                });
            }
        }));
        let mut streams: Vec<_> = (0..3)
            .map(|i| events(vec![end("", vec![call(&format!("repeat-{i}"), "tool")])]))
            .collect();
        streams.push(answer("done"));
        let agent = builder(
            Arc::new(Model::new(streams)),
            Arc::new(History::default()),
            LoopConfig::default(),
        )
        .tools(registry)
        .hooks(hooks.clone())
        .build();
        if yolo {
            agent
                .ctx()
                .set_security(Arc::new(yourai_harness::security::YoloSecurity));
        }
        assert_eq!(agent.run(In::user_text("go")).await.unwrap().text, "done");
        assert_eq!(
            h.inputs.lock().unwrap()[2],
            json!({"value":if yolo {1} else {2}})
        );
        assert_eq!(
            hooks
                .seen
                .lock()
                .unwrap()
                .iter()
                .filter(|kind| **kind == HookEventKind::PermissionRequest)
                .count(),
            if yolo { 0 } else { 1 }
        );
    }
}

#[tokio::test]
async fn managed_output_bounds_post_hook_mcp_results_and_preserves_original_file() {
    let d = tempfile::tempdir().unwrap();
    let store = yourai_harness::tools::ToolOutputStore::open(d.path().to_owned()).unwrap();
    let h = Arc::new(Handler::new("mcp__large", Mode::Return));
    let registry = Arc::new(Registry::default());
    registry.register(h);
    let hooks = Arc::new(Hooks::new(|_, r| {
        if let HookPointOutcome::PostToolUse(o) = &mut r.outcome {
            o.updated_mcp_tool_output = Some(json!({"ok":true,"content":"large".repeat(20_000)}));
        }
    }));
    let history = Arc::new(History::default());
    let agent = builder(
        Arc::new(Model::new(vec![calls(&["mcp__large"]), answer("done")])),
        history.clone(),
        LoopConfig {
            tool_output: Some(store),
            ..Default::default()
        },
    )
    .tools(registry)
    .hooks(hooks)
    .build();
    agent.run(In::user_text("go")).await.unwrap();
    let messages = history.messages();
    let response = messages
        .iter()
        .find(|m| m.role == ChatRole::Tool)
        .unwrap()
        .content
        .tool_responses()[0];
    let value: serde_json::Value = serde_json::from_str(&response.content).unwrap();
    assert_eq!(value["truncated"], true);
    let saved: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(value["output_paths"][0].as_str().unwrap()).unwrap(),
    )
    .unwrap();
    assert_eq!(saved["content"].as_str().unwrap().len(), 100_000);
}
