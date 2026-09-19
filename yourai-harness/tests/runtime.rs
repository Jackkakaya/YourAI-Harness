#[path = "support/context.rs"]
mod context_fixture;
use context_fixture::MemoryContext;
#[path = "../../yourai-loop/tests/support/mod.rs"]
mod support;
use serde_json::json;
use std::{sync::Arc, time::Duration};
use support::*;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*, runtime_event::RuntimeEvent};
use yourai_harness::{collaboration::*, *};
fn context(
    id: SessionId,
    model: Arc<dyn ModelProvider>,
    store: Arc<SqliteStore>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> Arc<MemoryContext> {
    let mut services = context_fixture::ContextServices::new(&id);
    services.store = Some(store.clone());
    services.hooks = hooks;
    services.usage = Some(Arc::new(storage::LocalUsage((*store).clone())));
    services.policy.context_window = Some(32_000);
    services.policy.keep_recent_tokens = 0;
    services.policy.summary_min_savings = 1;
    MemoryContext::new(id, model, services)
}
async fn host(
    dir: &std::path::Path,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> Arc<SessionHost> {
    let store = Arc::new(SqliteStore::open(&dir.join("sessions.sqlite3")).unwrap());
    let id = match store.list_sessions().await.unwrap().first() {
        Some(meta) => meta.id.clone(),
        None => store.create_session("").await.unwrap().id,
    };
    let history = context(id.clone(), model.clone(), store.clone(), hooks.clone());
    let mut b = Agent::builder()
        .agent_loop(Arc::new(yourai_loop::DefaultLoop::default()))
        .model(model)
        .context_manager(history)
        .session(store);
    if let Some(h) = hooks {
        b = b.hooks(h);
    }
    SessionHost::open(
        dir,
        SessionContext::new(id, dir),
        b.build(),
        HostConfig::default(),
        "startup",
    )
    .await
    .unwrap()
}
#[tokio::test]
async fn host_drives_durable_followups_and_close_is_idempotent() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![answer("one"), answer("two")])),
        Some(hooks.clone()),
    )
    .await;
    h.submit(In::user_text("first")).unwrap();
    h.submit(In::follow_up("next")).unwrap();
    let reports = h
        .run_until_idle(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(reports.len(), 2);
    assert!(reports
        .iter()
        .all(|r| r.result.as_ref().unwrap().pending.is_empty()));
    assert_eq!(h.status(), SessionStatus::Idle);
    assert!(h.close(Duration::from_secs(1)).await.unwrap().is_empty());
    assert!(h.close(Duration::from_secs(1)).await.unwrap().is_empty());
    assert!(h.submit(In::user_text("closed")).is_err());
    let seen = hooks.seen.lock().unwrap();
    assert!(seen.contains(&HookEventKind::SessionStart));
    assert_eq!(
        seen.iter()
            .filter(|k| **k == HookEventKind::SessionEnd)
            .count(),
        1
    );
}
#[tokio::test]
async fn dropping_driver_cancels_but_supervisor_preserves_followup() {
    let dir = TempDir::new().unwrap();
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![hangs_after("partial")])),
        None,
    )
    .await;
    h.submit(In::user_text("first")).unwrap();
    h.submit(In::follow_up("next")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let cloned = h.clone();
    let driver = tokio::spawn(async move {
        cloned
            .run_next(TurnLimits::default(), &tx, &CancellationToken::new())
            .await
    });
    while let Some(event) = rx.recv().await {
        if matches!(event, Out::Chunk { .. }) {
            break;
        }
    }
    assert!(h
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new()
        )
        .await
        .is_err());
    driver.abort();
    let _ = driver.await;
    tokio::time::timeout(Duration::from_secs(1), async {
        while h.status() != SessionStatus::Idle {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    assert_eq!(h.queued(), 1);
    assert_eq!(h.close(Duration::from_secs(1)).await.unwrap().len(), 1);
}
#[tokio::test]
async fn queued_input_restores_and_runtime_events_are_deduplicated() {
    let dir = TempDir::new().unwrap();
    let model = Arc::new(Model::new(vec![answer("done")]));
    {
        let h = host(dir.path(), model.clone(), None).await;
        h.submit(In::user_text("durable")).unwrap();
        let event = RuntimeEvent {
            id: "event-1".into(),
            context: Some("context".into()),
            notice: None,
            wake: false,
        };
        assert!(h.post_event(event.clone()).unwrap());
        assert!(!h.post_event(event).unwrap());
    }
    let h = host(dir.path(), model.clone(), None).await;
    assert_eq!(h.queued(), 1);
    h.run_next(
        TurnLimits::default(),
        &DiscardSink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(
        requests[0]
            .request
            .messages
            .iter()
            .filter(|m| m
                .content
                .first_text()
                .is_some_and(|t| t.contains("[Runtime event event-1]")))
            .count(),
        1
    );
}
#[tokio::test]
async fn startup_instructions_configuration_and_notifications_are_real_operations() {
    let dir = TempDir::new().unwrap();
    std::fs::write(dir.path().join("AGENTS.md"), "project instructions").unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![answer("ok")])),
        Some(hooks.clone()),
    )
    .await;
    let ws = h.workspace().unwrap();
    ws.setup("init").await.unwrap();
    ws.load_instructions(std::path::Path::new("AGENTS.md"), "startup")
        .await
        .unwrap();
    ws.change_config("project", json!({"system_prompt":"configured"}))
        .await
        .unwrap();
    assert_eq!(ws.config().unwrap()["system_prompt"], "configured");
    ws.notify("notice", "custom").await.unwrap();
    let child = dir.path().join("cwd");
    std::fs::create_dir(&child).unwrap();
    ws.change_cwd(&child).await.unwrap();
    assert_eq!(h.context().cwd, child.canonicalize().unwrap());
    h.submit(In::user_text("go")).unwrap();
    h.run_next(
        TurnLimits::default(),
        &DiscardSink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    let seen = hooks.seen.lock().unwrap();
    for kind in [
        HookEventKind::Setup,
        HookEventKind::InstructionsLoaded,
        HookEventKind::ConfigChange,
        HookEventKind::Notification,
        HookEventKind::CwdChanged,
    ] {
        assert!(seen.contains(&kind));
    }
}
#[tokio::test]
async fn collaboration_hooks_guard_task_transitions_and_tasks_persist() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|i, r| {
        if matches!(i.event, HookEvent::TaskCompleted { .. }) {
            block(r);
        }
    }));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let board = TaskBoard::new(&h, "team").unwrap();
    let task = board
        .create("work".into(), None, Some("alice".into()))
        .await
        .unwrap();
    assert!(board.complete(&task.id).await.is_err());
    assert!(!board.list()[0].completed);
    assert!(board.idle("alice").await.is_err());
    board.idle("bob").await.unwrap();
    assert_eq!(TaskBoard::new(&h, "team").unwrap().list().len(), 1);
    let seen = hooks.seen.lock().unwrap();
    for kind in [
        HookEventKind::TaskCreated,
        HookEventKind::TaskCompleted,
        HookEventKind::TeammateIdle,
    ] {
        assert!(seen.contains(&kind));
    }
}
#[tokio::test]
async fn file_watcher_reports_actual_changes_and_stops_on_close() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("watched.txt");
    std::fs::write(&path, "one").unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    h.watch_path(path.clone()).unwrap();
    let ws = h.workspace().unwrap();
    ws.start_watching(Duration::from_millis(5)).unwrap();
    tokio::time::sleep(Duration::from_millis(20)).await;
    std::fs::write(path, "two").unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while !hooks
            .seen
            .lock()
            .unwrap()
            .contains(&HookEventKind::FileChanged)
        {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    h.close(Duration::from_secs(1)).await.unwrap();
}
#[tokio::test]
async fn worktrees_are_created_and_removed_by_git() {
    let repo = TempDir::new().unwrap();
    for args in [
        vec!["init"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "init",
        ],
    ] {
        assert!(std::process::Command::new("git")
            .args(args)
            .current_dir(repo.path())
            .output()
            .unwrap()
            .status
            .success());
    }
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let ws = h.workspace().unwrap();
    ws.change_cwd(repo.path()).await.unwrap();
    let p = ws.create_worktree("test").await.unwrap();
    assert!(p.join(".git").exists());
    ws.remove_worktree("test").await.unwrap();
    assert!(!p.exists());
    let seen = hooks.seen.lock().unwrap();
    assert!(seen.contains(&HookEventKind::WorktreeCreate));
    assert!(seen.contains(&HookEventKind::WorktreeRemove));
}
#[tokio::test]
async fn child_agent_runs_its_own_session_and_reports_lifecycle() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let child_model = Arc::new(Model::new(vec![answer("child result")]));
    let tool = SubagentTool::new(&h, child_model, None);
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        call_id: "child-tool".into(),
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    let result = tool.execute(tc, json!({"prompt":"do work"})).await.unwrap();
    assert_eq!(result["text"], "child result");
    assert_eq!(tool.child_ids().len(), 1);
    let seen = hooks.seen.lock().unwrap();
    assert!(seen.contains(&HookEventKind::SubagentStart));
    assert!(seen.contains(&HookEventKind::SubagentStop));
}
#[tokio::test]
async fn crash_recovery_closes_unmatched_calls_without_executing() {
    let dir = TempDir::new().unwrap();
    let model = Arc::new(Model::new(vec![]));
    let h = host(dir.path(), model.clone(), None).await;
    // Persist a call with no result, as if the process stopped after dispatch.
    let store = SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap();
    let id = store.list_sessions().await.unwrap()[0].id.clone();
    store
        .append_messages(
            &id,
            vec![StoredMessage::new(ChatMessage::from(vec![call(
                "c", "write",
            )]))],
        )
        .await
        .unwrap();
    h.close(Duration::from_secs(1)).await.unwrap();
    drop(h);
    let recovered = host(dir.path(), model, None).await;
    let rows = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(rows.len(), 2);
    assert!(rows[1].message.content.tool_responses()[0]
        .content
        .contains("unknown"));
    recovered.close(Duration::from_secs(1)).await.unwrap();
}

struct SummaryModel;
impl ModelProvider for SummaryModel {
    fn model_iden(&self) -> &str {
        "summary-model"
    }
    fn complete<'a>(&'a self, _: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async {
            let id = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "summary-model");
            Ok(ChatResponse {
                content: MessageContent::from_text("summary retains completed work"),
                reasoning_content: None,
                model_iden: id.clone(),
                provider_model_iden: id,
                stop_reason: None,
                usage: GenaiUsage {
                    prompt_tokens: Some(10),
                    completion_tokens: Some(3),
                    total_tokens: Some(13),
                    ..Default::default()
                },
                captured_raw_body: None,
                response_id: None,
            })
        })
    }
    fn stream_events<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async { Err(ErrorKind::Config("unused".into()).into()) })
    }
}
#[tokio::test]
async fn real_compaction_commits_summary_but_retains_transcript_and_identities() {
    let dir = TempDir::new().unwrap();
    let model = Arc::new(SummaryModel);
    let store = Arc::new(SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap());
    let id = store.create_session("instructions").await.unwrap().id;
    let memory = context(id.clone(), model, store.clone(), None);
    let history = memory.clone();
    history.restore().await.unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user(
            "long conversation".repeat(100),
        ))])
        .await
        .unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::from(vec![call(
            "c1", "tool",
        )]))])
        .await
        .unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::from(
            ToolResponse::new("c1", "done"),
        ))])
        .await
        .unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user(
            "current question",
        ))])
        .await
        .unwrap();
    let before = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages
        .len();
    let result = history
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.usage.unwrap().total_tokens, 13);
    assert_eq!(
        store
            .read_messages(&id, MessageQuery::default())
            .await
            .unwrap()
            .messages
            .len(),
        before + 1
    );
    assert!(history.contains_tool_call("c1"));
    assert_eq!(history.messages().len(), 2);
    assert_eq!(
        history
            .build_request(&[])
            .unwrap()
            .request
            .system
            .as_deref(),
        Some("instructions")
    );
}
#[tokio::test]
async fn manual_compact_emits_hooks_and_accounting() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(dir.path(), Arc::new(SummaryModel), Some(hooks.clone())).await;
    let store = SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap();
    let id = store.list_sessions().await.unwrap()[0].id.clone();
    store
        .append_messages(
            &id,
            vec![
                StoredMessage::new(ChatMessage::user("summarize completed work".repeat(100))),
                StoredMessage::new(ChatMessage::user("current question")),
            ],
        )
        .await
        .unwrap();
    let result = h
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.usage.unwrap().total_tokens, 13);
    assert_eq!(h.status(), SessionStatus::Idle);
    let seen = hooks.seen.lock().unwrap();
    assert!(seen.contains(&HookEventKind::PreCompact));
    assert!(seen.contains(&HookEventKind::PostCompact));
}
#[tokio::test]
async fn shared_budget_counts_main_stream_and_compaction_model() {
    let budget = ModelBudget::new(Some(2), None);
    let streaming = MeteredModel {
        inner: Arc::new(Model::new(vec![answer("hi")])),
        budget: budget.clone(),
    };
    let summary = MeteredModel {
        inner: Arc::new(SummaryModel),
        budget: budget.clone(),
    };
    use futures_util::StreamExt;
    let request = ModelRequest::new(ChatRequest::from_user("go"), ChatOptions::default());
    let mut stream = streaming.stream_events(request.clone()).await.unwrap();
    while stream.next().await.is_some() {}
    summary.complete(request.clone()).await.unwrap();
    assert!(summary.complete(request).await.is_err());
    let snapshot = budget.snapshot();
    assert_eq!(snapshot.calls, 2);
    assert_eq!(snapshot.usage.total_tokens, 16);
}
#[tokio::test]
async fn async_hook_completion_routes_to_own_session_and_wakes_host() {
    let dir = TempDir::new().unwrap();
    let runtime = Arc::new(yourai_harness::hooks::ConcreteHookRuntime::new());
    let config:yourai_harness::hooks::HooksConfig=serde_json::from_value(json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"echo background-feedback >&2; exit 2","asyncRewake":true}]}]}})).unwrap();
    runtime
        .register_config(&config, HookSource::Session)
        .await
        .unwrap();
    let model = Arc::new(Model::new(vec![answer("handled")]));
    let h = host(dir.path(), model.clone(), Some(runtime)).await;
    tokio::time::timeout(Duration::from_secs(2), async {
        while h.queued() == 0 {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    h.run_next(
        TurnLimits::default(),
        &DiscardSink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert!(model.requests.lock().unwrap()[0]
        .request
        .messages
        .iter()
        .any(|m| m
            .content
            .first_text()
            .is_some_and(|t| t.contains("background-feedback"))));
    h.close(Duration::from_secs(1)).await.unwrap();
}
#[tokio::test]
async fn catalog_create_fork_list_delete_are_persistent() {
    let dir = TempDir::new().unwrap();
    let catalog = SessionCatalog::new(dir.path()).unwrap();
    let first = catalog.create_session("").await.unwrap();
    let fork = catalog.fork_session(&first.id).await.unwrap();
    assert_eq!(catalog.list_sessions().await.unwrap().len(), 2);
    catalog.delete_session(&fork).await.unwrap();
    assert_eq!(catalog.load_session(&first.id).await.unwrap().id, first.id);
}

#[tokio::test]
async fn concurrent_close_runs_session_end_once_and_releases_history_lock() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    h.submit(In::follow_up("handoff")).unwrap();
    let (a, b) = tokio::join!(
        h.close(Duration::from_secs(1)),
        h.close(Duration::from_secs(1))
    );
    assert_eq!(a.unwrap().len() + b.unwrap().len(), 1);
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|k| **k == HookEventKind::SessionEnd)
            .count(),
        1
    );
    // Reopen with the original Arc still alive: explicit close must release both locks.
    let next = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    assert_eq!(next.status(), SessionStatus::Idle);
    next.close(Duration::from_secs(1)).await.unwrap();
}
#[tokio::test]
async fn permission_updates_are_atomic_persistent_and_cannot_expand_scope() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("permissions.json");
    let policy = PolicySecurity::open(path.clone(), vec!["danger".into()]).unwrap();
    let grant = json!({"type":"addRules","destination":"session","behavior":"allow","rules":[{"toolName":"safe"},{"toolName":"danger"}]});
    policy.update_permissions(&[grant]).await.unwrap();
    let context = |name: &str| SecurityContext {
        action: name.into(),
        input: json!({}),
        is_destructive: false,
        is_network: false,
    };
    assert!(matches!(
        policy.check_tool_call(&context("danger")).await.unwrap(),
        ApprovalDecision::Deny
    ));
    let invalid =
        json!({"type":"replaceRules","destination":"userSettings","behavior":"allow","rules":[]});
    assert!(policy.update_permissions(&[invalid]).await.is_err());
    let policy = PolicySecurity::open(path, vec![]).unwrap();
    assert!(matches!(
        policy.check_tool_call(&context("safe")).await.unwrap(),
        ApprovalDecision::Allow
    ));
    let scoped = json!({"type":"replaceRules","behavior":"allow","rules":[{"toolName":"safe","ruleContent":"restricted"}]});
    assert!(policy.update_permissions(&[scoped]).await.is_err());
    assert!(matches!(
        policy.check_tool_call(&context("safe")).await.unwrap(),
        ApprovalDecision::Allow
    ));
}
#[tokio::test]
async fn harness_runs_real_task_tool_through_loop_and_restores_session() {
    let dir = TempDir::new().unwrap();
    let mut call = call("create-task", "tasks");
    call.fn_arguments = json!({"action":"create","subject":"verify full flow"});
    let model = Arc::new(Model::new(vec![
        events(vec![end("", vec![call])]),
        answer("created"),
    ]));
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.extensions = true;
    config.hooks=serde_json::from_value(json!({"hooks":{"PermissionRequest":[{"hooks":[{"type":"command","command":"echo '{\"hookSpecificOutput\":{\"hookEventName\":\"PermissionRequest\",\"decision\":{\"behavior\":\"allow\"}}}'"}]}]}})).unwrap();
    let harness = Harness::open(config, model).await.unwrap();
    let frozen = SessionCatalog::new(dir.path().join("sessions"))
        .unwrap()
        .load_session(&harness.host.context().id)
        .await
        .unwrap()
        .system_prompt
        .unwrap();
    harness
        .workspace
        .as_ref()
        .unwrap()
        .change_config("project", json!({"system_prompt":"runtime-config"}))
        .await
        .unwrap();
    harness.host.submit(In::user_text("create a task")).unwrap();
    let report = harness
        .host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(report.result.unwrap().text, "created");
    assert_eq!(harness.tasks.as_ref().unwrap().list().len(), 1);
    assert_eq!(harness.budget.snapshot().calls, 2);
    assert_eq!(harness.usage.total().await.unwrap().total_tokens, 6);
    let id = harness.host.context().id;
    harness.close().await.unwrap();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.resume = Some(id);
    config.extensions = true;
    let resumed_model = Arc::new(Model::new(vec![answer("resumed")]));
    let resumed = Harness::open(config, resumed_model.clone()).await.unwrap();
    assert_eq!(resumed.tasks.as_ref().unwrap().list().len(), 1);
    resumed.host.submit(In::user_text("continue")).unwrap();
    resumed
        .host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(
        resumed_model.requests.lock().unwrap()[0]
            .request
            .system
            .as_deref(),
        Some(frozen.as_str())
    );
    resumed.close().await.unwrap();
}
#[tokio::test]
async fn agent_hook_uses_real_loop_and_shared_budget() {
    use yourai_harness::hooks::{HookModelExecutor, HookModelRequest};
    let budget = ModelBudget::new(Some(1), None);
    let executor = assembly::model_hooks::DefaultHookModelExecutor {
        model: Arc::new(MeteredModel {
            inner: Arc::new(Model::new(vec![answer(
                r#"{"ok":false,"reason":"unfinished"}"#,
            )])),
            budget: budget.clone(),
        }),
        tools: None,
        usage: None,
        timeout: Duration::from_secs(1),
        max_model_calls: 2,
    };
    let request = HookModelRequest {
        prompt: "evaluate".into(),
        model: None,
        agentic: true,
        invocation: HookInvocation::new(
            BaseInput::new("session", "."),
            HookEvent::SessionEnd {
                reason: "shutdown".into(),
            },
        ),
    };
    let result = executor.evaluate(request.clone()).await.unwrap();
    assert!(!result.ok);
    assert_eq!(result.reason.as_deref(), Some("unfinished"));
    assert_eq!(budget.snapshot().calls, 1);
    assert!(executor.evaluate(request).await.is_err());
}
#[tokio::test]
async fn watched_directory_detects_created_and_deleted_children() {
    let dir = TempDir::new().unwrap();
    let watched = dir.path().join("watched");
    std::fs::create_dir(&watched).unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    h.watch_path(watched.clone()).unwrap();
    tokio::time::sleep(Duration::from_millis(300)).await;
    std::fs::create_dir(watched.join("nested")).unwrap();
    let file = watched.join("nested/new.txt");
    std::fs::write(&file, "new").unwrap();
    for expected in 1..=2 {
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if hooks
                    .seen
                    .lock()
                    .unwrap()
                    .iter()
                    .filter(|k| **k == HookEventKind::FileChanged)
                    .count()
                    >= expected
                {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        if expected == 1 {
            std::fs::remove_file(&file).unwrap();
        }
    }
    h.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn closing_session_cancels_its_background_command() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("should-not-exist");
    let runtime = Arc::new(yourai_harness::hooks::ConcreteHookRuntime::new());
    let config:yourai_harness::hooks::HooksConfig=serde_json::from_value(json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":format!("sleep 0.3; touch '{}'",marker.display()),"async":true}]}]}})).unwrap();
    runtime
        .register_config(&config, HookSource::Session)
        .await
        .unwrap();
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(runtime.clone()),
    )
    .await;
    h.close(Duration::from_secs(1)).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn shared_hook_runtime_keeps_background_contexts_session_local() {
    let dir = TempDir::new().unwrap();
    let runtime = Arc::new(yourai_harness::hooks::ConcreteHookRuntime::new());
    let config:yourai_harness::hooks::HooksConfig=serde_json::from_value(json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"sleep 0.1; echo feedback >&2; exit 2","asyncRewake":true}]}]}})).unwrap();
    runtime
        .register_config(&config, HookSource::Session)
        .await
        .unwrap();
    let a = Arc::new(Model::new(vec![answer("a")]));
    let b = Arc::new(Model::new(vec![answer("b")]));
    let first = SessionHost::create(
        dir.path(),
        dir.path().into(),
        a.clone(),
        Some(runtime.clone()),
        None,
    )
    .await
    .unwrap();
    let second = SessionHost::create(
        dir.path(),
        dir.path().into(),
        b.clone(),
        Some(runtime),
        None,
    )
    .await
    .unwrap();
    tokio::time::sleep(Duration::from_millis(250)).await;
    for h in [&first, &second] {
        assert_eq!(h.queued(), 1);
        h.run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
        h.close(Duration::from_secs(1)).await.unwrap();
    }
    for model in [a, b] {
        assert_eq!(
            model.requests.lock().unwrap()[0]
                .request
                .messages
                .iter()
                .filter(|m| m
                    .content
                    .first_text()
                    .is_some_and(|s| s.contains("feedback")))
                .count(),
            1
        );
    }
}

#[tokio::test]
async fn failed_storage_never_changes_memory_context() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap());
    let id = store.create_session("").await.unwrap().id;
    let memory = context(id.clone(), Arc::new(SummaryModel), store.clone(), None);
    let history = memory.clone();
    history.restore().await.unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user("original"))])
        .await
        .unwrap();
    let before = serde_json::to_string(&memory.messages()).unwrap();
    // Deleting the session makes a subsequent write fail its FK constraint.
    store.delete_session(&id).await.unwrap();
    assert!(history
        .append(vec![StoredMessage::new(ChatMessage::user(
            "must not enter memory"
        ))])
        .await
        .is_err());
    assert_eq!(before, serde_json::to_string(&memory.messages()).unwrap());
}

#[tokio::test]
async fn restored_compacted_context_matches_committed_model_view() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap());
    let id = store.create_session("").await.unwrap().id;
    let memory = context(id.clone(), Arc::new(SummaryModel), store.clone(), None);
    let history = memory.clone();
    history.restore().await.unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user(
            "old conversation".repeat(100),
        ))])
        .await
        .unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user(
            "current question",
        ))])
        .await
        .unwrap();
    history
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    history
        .append(vec![StoredMessage::new(ChatMessage::user("new input"))])
        .await
        .unwrap();
    let restored = context(id, Arc::new(SummaryModel), store.clone(), None);
    restored.restore().await.unwrap();
    assert_eq!(
        serde_json::to_value(memory.messages()).unwrap(),
        serde_json::to_value(restored.messages()).unwrap()
    );
    assert!(restored.messages()[0]
        .content
        .first_text()
        .unwrap()
        .contains("summary"));
}

#[tokio::test]
async fn basic_harness_has_todos_without_optional_extensions() {
    let dir = TempDir::new().unwrap();
    let h = Harness::open(
        HarnessConfig::new(dir.path().join("sessions"), dir.path().into()),
        Arc::new(Model::new(vec![answer("ok")])),
    )
    .await
    .unwrap();
    assert!(
        h.workspace.is_none()
            && h.tasks.is_some()
            && h.subagents.is_none()
            && h.memory.is_none()
            && h.skills.is_none()
    );
    assert!(
        h.tools.has("tasks")
            && !h.tools.has("subagent")
            && h.tools.has("read")
            && h.tools.has("write")
            && h.tools.has("edit")
            && h.tools.has("shell")
            && !h.tools.has("read_asset")
    );
    h.host.submit(In::user_text("hello")).unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    let session = dir.path().join("sessions").join(h.host.context().id.0);
    assert!(
        !session.join("memory.json").exists()
            && !session.join("skills.json").exists()
            && !session.join("config.json").exists()
    );
    h.close().await.unwrap();
}

struct CountSummary(std::sync::atomic::AtomicUsize);
impl ModelProvider for CountSummary {
    fn model_iden(&self) -> &str {
        "count-summary"
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        SummaryModel.complete(request)
    }
    fn stream_events<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async { Err(ErrorKind::Config("unused".into()).into()) })
    }
}
#[tokio::test]
async fn manual_compact_uses_current_execution_providers_and_frozen_system() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap());
    let system = "frozen system ".repeat(40);
    let id = store.create_session(&system).await.unwrap().id;
    let old = Arc::new(CountSummary(AtomicUsize::new(0)));
    let new = Arc::new(CountSummary(AtomicUsize::new(0)));
    let history = context(id.clone(), old.clone(), store.clone(), None);
    let old_hooks = Arc::new(Hooks::new(|_, _| {}));
    let new_hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = Agent::builder()
        .model(old.clone())
        .context_manager(history.inner.clone())
        .hooks(old_hooks.clone())
        .agent_loop(Arc::new(yourai_loop::DefaultLoop::default()))
        .build();
    let h = SessionHost::open(
        dir.path(),
        SessionContext::new(id.clone(), dir.path()),
        agent.clone(),
        HostConfig::default(),
        "startup",
    )
    .await
    .unwrap();
    history
        .append(vec![
            StoredMessage::new(ChatMessage::user("past work ".repeat(1000))),
            StoredMessage::new(ChatMessage::user("latest")),
        ])
        .await
        .unwrap();
    agent.ctx().set_model(new.clone());
    agent.ctx().set_hooks(new_hooks.clone());
    let usage = Arc::new(storage::LocalUsage((*store).clone()));
    agent.ctx().set_usage(usage.clone());
    let execution =
        ContextExecution::from_snapshot(&agent.ctx().snapshot().unwrap(), &id, Some(&h.context()))
            .unwrap();
    let before = history
        .inner
        .build_request(&[], &execution)
        .unwrap()
        .estimated_tokens;
    let result = h
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.tokens_before, before);
    assert_eq!(result.action, CompactAction::Summarized);
    assert_eq!(old.0.load(Ordering::SeqCst), 0);
    assert_eq!(new.0.load(Ordering::SeqCst), 1);
    assert!(!old_hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PreCompact));
    assert!(new_hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PreCompact));
    assert_eq!(usage.session_usage(&id).await.unwrap().request_count, 1);
    h.close(Duration::from_secs(1)).await.unwrap();
}

#[tokio::test]
async fn context_usage_estimates_active_request_without_calling_model() {
    let dir = TempDir::new().unwrap();
    let model = Arc::new(Model::new(vec![answer("done")]));
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.context_policy.context_window = Some(32_000);
    config.system_prompt = Some("You are a coding assistant.".into());
    let h = Harness::open(config, model.clone()).await.unwrap();
    let before = h.host.context_usage().unwrap();
    assert_eq!(before.context_window, Some(32_000));
    assert_eq!(before.input_budget, Some(32_000 - 4096 - 1024));
    assert!(before.estimated_tokens > 0); // system prompt + registered tool schemas
    assert!(model.requests.lock().unwrap().is_empty());
    h.host
        .submit(In::user_text("Explain this code in detail.".repeat(100)))
        .unwrap();
    h.host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    let after = h.host.context_usage().unwrap();
    // The estimate is recalibrated against the mock provider usage after a turn.
    assert!(after.estimated_tokens > 0);
    assert_ne!(after.estimated_tokens, before.estimated_tokens);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    h.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn rate_limit_attempts_stop_at_retry_limit() {
    struct Limited(std::sync::atomic::AtomicUsize);
    impl ModelProvider for Limited {
        fn model_iden(&self) -> &str {
            "limited"
        }
        fn complete<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async { unreachable!() })
        }
        fn stream_events<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            Box::pin(async move {
                self.0.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err(ErrorKind::Model {
                    source: genai::Error::HttpError {
                        status: "429".parse().unwrap(),
                        canonical_reason: "Too Many Requests".into(),
                        body: r#"{"error":{"code":"rate_limit_exceeded"}}"#.into(),
                    },
                }
                .into())
            })
        }
    }
    let provider = Arc::new(Limited(std::sync::atomic::AtomicUsize::new(0)));
    let budget = ModelBudget::new(None, None);
    let model = Arc::new(MeteredModel {
        inner: provider.clone(),
        budget: budget.clone(),
    });
    let agent = Agent::builder()
        .agent_loop(Arc::new(yourai_loop::DefaultLoop::new(
            yourai_loop::LoopConfig {
                retry_delay: Duration::ZERO,
                ..Default::default()
            },
        )))
        .model(model)
        .context_manager(Arc::new(History::default()))
        .build();
    assert!(agent.run(In::user_text("hello")).await.is_err());
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 3);
    let metrics = budget.snapshot();
    assert_eq!(metrics.calls, 3);
    assert_eq!(metrics.requests.rate_limited, 3);
    assert_eq!(metrics.requests.active, 0);
    assert!(yourai_loop::LoopConfig::default().retry_delay >= Duration::from_secs(5));
}
