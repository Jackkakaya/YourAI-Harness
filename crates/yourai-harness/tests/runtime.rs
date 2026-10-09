#[path = "support/context.rs"]
mod context_fixture;
use context_fixture::DefaultContext;
#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::{
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};
use support::Model;
use support::*;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::runtime::SessionLease;
use yourai_core::{
    context::DiscardSink, hooks::GenericOutcome, prelude::*, runtime_event::RuntimeEvent,
};
use yourai_harness::{collaboration::*, *};

#[test]
fn default_agent_budgets_are_unlimited() {
    let loop_config = default_loop::LoopConfig::default();
    assert_eq!(loop_config.steps, None);
    // OpenCode attachment.image defaults: 5 MiB base64 / 2000x2000 / resize.
    assert!(loop_config.execution.attachment_image.auto_resize);
    assert_eq!(loop_config.execution.attachment_image.max_width, 2000);
    assert_eq!(loop_config.execution.attachment_image.max_height, 2000);
    assert_eq!(
        loop_config.execution.attachment_image.max_base64_bytes,
        5 * 1024 * 1024
    );
    // Text files attached by reference are capped at 50k chars.
    assert_eq!(loop_config.execution.attachment_text_max_chars, 50_000);
}

fn context(
    id: SessionId,
    model: Arc<dyn ModelProvider>,
    store: Arc<SqliteStore>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> Arc<DefaultContext> {
    let mut services = context_fixture::ContextServices::new(&id);
    services.store = Some(store.clone());
    services.hooks = hooks;
    services.usage = Some(Arc::new(storage::LocalUsage((*store).clone())));
    services.policy.context_window = Some(32_000);
    services.policy.keep_recent_tokens = 0;
    services.policy.summary_min_savings = 1;
    DefaultContext::new(id, model, services)
}
async fn host(
    dir: &std::path::Path,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> Arc<SessionHost> {
    let store = Arc::new(SqliteStore::open(&dir.join("sessions.sqlite3")).unwrap());
    let id = match store.list_sessions().await.unwrap().first() {
        Some(meta) => meta.id.clone(),
        None => store.create_session(SessionId::new(), "").await.unwrap().id,
    };
    let history = context(id.clone(), model.clone(), store.clone(), hooks.clone());
    let mut b = Agent::builder()
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
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
    assert!(h
        .close(Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .is_empty());
    assert!(h
        .close(Some(Duration::from_secs(1)))
        .await
        .unwrap()
        .is_empty());
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
    assert_eq!(
        h.close(Some(Duration::from_secs(1))).await.unwrap().len(),
        1
    );
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
    ws.change_config("project", json!({"memory_search_limit":4}))
        .await
        .unwrap();
    assert_eq!(ws.config().unwrap()["memory_search_limit"], 4);
    assert!(ws
        .change_config("project", json!({"system_prompt":"unsupported"}))
        .await
        .is_err());
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
    let denied = Arc::new(AtomicBool::new(true));
    let denied_hook = denied.clone();
    let hooks = Arc::new(Hooks::new(move |i, r| {
        if matches!(
            i.event,
            HookEvent::TaskCompleted { .. } | HookEvent::TeammateIdle { .. }
        ) && denied_hook.load(Ordering::SeqCst)
        {
            block(r);
        }
    }));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let manager = h.task_manager("team").unwrap();
    let task = manager
        .create("work".into(), None, Some("alice".into()))
        .await
        .unwrap();
    assert!(manager.complete(&task.id).await.is_err());
    assert!(!manager.list()[0].completed);
    assert!(manager.idle("alice").await.is_err());
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::TeammateIdle));
    assert!(manager.idle("bob").await.is_err());
    denied.store(false, Ordering::SeqCst);
    manager.complete(&task.id).await.unwrap();
    let seen_count = hooks.seen.lock().unwrap().len();
    manager.complete(&task.id).await.unwrap();
    assert_eq!(hooks.seen.lock().unwrap().len(), seen_count);
    assert_eq!(manager.version(), 2);
    manager.idle("alice").await.unwrap();
    {
        let seen = hooks.seen.lock().unwrap();
        for kind in [
            HookEventKind::TaskCreated,
            HookEventKind::TaskCompleted,
            HookEventKind::TeammateIdle,
        ] {
            assert!(seen.contains(&kind));
        }
    }
    h.close(None).await.unwrap();
    let reopened = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    assert!(reopened.task_manager("team").unwrap().list()[0].completed);
    reopened.close(None).await.unwrap();
}
#[tokio::test]
async fn task_manager_is_shared_and_creation_order_survives_restart() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
    let manager = h.task_manager("team").unwrap();
    for subject in ["first", "second", "third"] {
        manager.create(subject.into(), None, None).await.unwrap();
    }
    let subjects = |b: &TaskManager| b.list().into_iter().map(|t| t.subject).collect::<Vec<_>>();
    // Creation order, not the random UUID order of the underlying map.
    assert_eq!(subjects(&manager), ["first", "second", "third"]);
    let shared = h.task_manager("team").unwrap();
    assert!(Arc::ptr_eq(&manager, &shared));
    assert!(h.task_manager("another team").is_err());
    shared.create("fourth".into(), None, None).await.unwrap();
    assert_eq!(subjects(&manager), ["first", "second", "third", "fourth"]);
    assert!(!dir.path().join("tasks.json").exists());
    h.close(None).await.unwrap();
    let reopened = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let reloaded = reopened.task_manager("team").unwrap();
    assert_eq!(subjects(&reloaded), ["first", "second", "third", "fourth"]);
    reloaded.create("fifth".into(), None, None).await.unwrap();
    assert_eq!(reloaded.list()[4].seq, 5);
    // Completion preserves creation order.
    reloaded.complete(&reloaded.list()[0].id).await.unwrap();
    assert_eq!(
        subjects(&reloaded),
        ["first", "second", "third", "fourth", "fifth"]
    );
    assert!(reloaded.list()[0].completed);
    reopened.close(None).await.unwrap();
}
#[tokio::test]
async fn legacy_task_file_is_imported_and_cannot_overwrite_sqlite_completion() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    // Files written before `seq` existed load with 0 and keep id order.
    let legacy = r#"{"b":{"id":"b","subject":"legacy-b","description":null,"owner":null,"completed":false},
                     "a":{"id":"a","subject":"legacy-a","description":null,"owner":null,"completed":false}}"#;
    let path = dir.path().join("tasks.json");
    std::fs::write(&path, legacy).unwrap();
    let manager = h.task_manager("team").unwrap();
    let subjects = |m: &TaskManager| m.list().into_iter().map(|t| t.subject).collect::<Vec<_>>();
    assert_eq!(subjects(&manager), ["legacy-a", "legacy-b"]);
    manager.complete("a").await.unwrap();
    manager.create("fresh".into(), None, None).await.unwrap();
    assert_eq!(subjects(&manager), ["legacy-a", "legacy-b", "fresh"]);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
    h.close(None).await.unwrap();
    let reopened = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let reloaded = reopened.task_manager("team").unwrap();
    assert_eq!(subjects(&reloaded), ["legacy-a", "legacy-b", "fresh"]);
    assert!(reloaded.list()[0].completed);
    assert_eq!(std::fs::read_to_string(&path).unwrap(), legacy);
    reopened.close(None).await.unwrap();
}
#[tokio::test]
async fn invalid_legacy_task_file_leaves_no_partial_import() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let legacy = r#"{"a":{"id":"a","subject":"valid","description":null,"owner":null,"completed":false},
                     "b":{"id":"wrong-key","subject":"invalid","description":null,"owner":null,"completed":false}}"#;
    std::fs::write(dir.path().join("tasks.json"), legacy).unwrap();
    assert!(h.task_manager("team").is_err());
    assert!(h
        .agent()
        .ctx()
        .session()
        .unwrap()
        .read_tasks(&h.context().id)
        .unwrap()
        .is_empty());
    h.close(None).await.unwrap();
}
#[tokio::test]
async fn blocked_task_create_leaves_no_row_and_releases_the_transition_lock() {
    let dir = TempDir::new().unwrap();
    let denied = Arc::new(AtomicBool::new(true));
    let denied_hook = denied.clone();
    let hooks = Arc::new(Hooks::new(move |i, r| {
        if i.event.kind() == HookEventKind::TaskCreated && denied_hook.load(Ordering::SeqCst) {
            block(r);
        }
    }));
    let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
    let manager = h.task_manager("team").unwrap();
    assert!(manager.create("blocked".into(), None, None).await.is_err());
    assert_eq!(manager.version(), 0);
    assert!(manager.list().is_empty());
    assert!(h
        .agent()
        .ctx()
        .session()
        .unwrap()
        .read_tasks(&h.context().id)
        .unwrap()
        .is_empty());
    denied.store(false, Ordering::SeqCst);
    let task = tokio::time::timeout(
        Duration::from_secs(1),
        manager.create("accepted".into(), None, None),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(task.seq, 1);
    assert_eq!(manager.version(), 1);
    h.close(None).await.unwrap();
}
#[tokio::test]
async fn invalid_task_hook_or_failed_effect_storage_prevents_task_save() {
    for invalid in [false, true] {
        let dir = TempDir::new().unwrap();
        let hooks = Arc::new(Hooks::new(move |i, r| {
            if i.event.kind() == HookEventKind::TaskCreated {
                r.outcome = HookPointOutcome::Generic(GenericOutcome {
                    additional_contexts: vec!["context".into()],
                });
                if invalid {
                    r.event = HookEventKind::Stop;
                }
            }
        }));
        let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
        let manager = h.task_manager("team").unwrap();
        let journal = h.directory().join("host.json");
        if !invalid {
            std::fs::remove_file(&journal).unwrap();
            std::fs::create_dir(&journal).unwrap();
        }
        assert!(manager.create("subject".into(), None, None).await.is_err());
        assert!(manager.list().is_empty());
        assert_eq!(manager.version(), 0);
        assert!(h
            .agent()
            .ctx()
            .session()
            .unwrap()
            .read_tasks(&h.context().id)
            .unwrap()
            .is_empty());
        if !invalid {
            std::fs::remove_dir(journal).unwrap();
        }
        h.close(None).await.unwrap();
    }
}
#[tokio::test]
async fn concurrent_task_creates_have_unique_sequences_and_no_lost_rows() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let manager = h.task_manager("team").unwrap();
    let mut handles = vec![];
    for n in 0..32 {
        let shared = h.task_manager("team").unwrap();
        handles.push(tokio::spawn(async move {
            shared.create(n.to_string(), None, None).await.unwrap()
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    assert_eq!(
        manager.list().iter().map(|t| t.seq).collect::<Vec<_>>(),
        (1..=32).collect::<Vec<_>>()
    );
    assert_eq!(manager.version(), 32);
    assert_eq!(
        h.agent()
            .ctx()
            .session()
            .unwrap()
            .read_tasks(&h.context().id)
            .unwrap(),
        manager.list()
    );
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn failed_task_commit_does_not_publish_cache_or_consume_sequence() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let manager = h.task_manager("team").unwrap();
    let db = rusqlite::Connection::open(SqliteStore::path(dir.path())).unwrap();
    db.execute_batch("CREATE TRIGGER reject_task BEFORE INSERT ON tasks BEGIN SELECT RAISE(ABORT, 'test failure'); END;").unwrap();
    assert!(manager.create("failed".into(), None, None).await.is_err());
    assert!(manager.list().is_empty());
    assert_eq!(manager.version(), 0);
    db.execute_batch("DROP TRIGGER reject_task;").unwrap();
    let saved = manager.create("accepted".into(), None, None).await.unwrap();
    assert_eq!(saved.seq, 1);
    assert_eq!(manager.version(), 1);
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn dropping_task_save_waiter_still_finishes_commit_and_cache_publication() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let manager = h.task_manager("team").unwrap();
    let db = rusqlite::Connection::open(SqliteStore::path(dir.path())).unwrap();
    db.execute_batch("BEGIN IMMEDIATE;").unwrap();
    let owners = Arc::strong_count(&manager);
    let caller = manager.clone();
    let write = tokio::spawn(async move { caller.create("durable".into(), None, None).await });
    // Wait until both the caller and the owned save worker hold the manager.
    // The external SQLite write lock keeps that worker from committing yet.
    tokio::time::timeout(Duration::from_secs(1), async {
        while Arc::strong_count(&manager) < owners + 2 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    write.abort();
    assert!(write.await.unwrap_err().is_cancelled());
    db.execute_batch("COMMIT;").unwrap();
    tokio::time::timeout(Duration::from_secs(1), async {
        while manager.version() == 0 {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let rows = h
        .agent()
        .ctx()
        .session()
        .unwrap()
        .read_tasks(&h.context().id)
        .unwrap();
    assert_eq!(rows, manager.list());
    assert_eq!(rows[0].subject, "durable");
    // The transition lock also leaves with the worker, allowing the next save.
    assert_eq!(
        manager.create("next".into(), None, None).await.unwrap().seq,
        2
    );
    h.close(None).await.unwrap();
}

struct PausedTaskHook {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl HookRuntime for PausedTaskHook {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            if invocation.event.kind() == HookEventKind::TaskCreated {
                self.entered.notify_one();
                self.release.notified().await;
            }
            Ok(HookDispatchResult::empty(invocation.event.kind()))
        })
    }
}

#[tokio::test]
async fn close_during_task_hook_prevents_a_late_write_by_the_old_host() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(PausedTaskHook {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let manager = h.task_manager("team").unwrap();
    let creator = manager.clone();
    let write = tokio::spawn(async move { creator.create("late".into(), None, None).await });
    tokio::time::timeout(Duration::from_secs(1), hooks.entered.notified())
        .await
        .unwrap();
    h.close(None).await.unwrap();
    let reopened = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    reopened
        .task_manager("team")
        .unwrap()
        .create("new owner".into(), None, None)
        .await
        .unwrap();
    hooks.release.notify_one();
    assert!(write.await.unwrap().is_err());
    assert!(manager.list().is_empty());
    assert_eq!(reopened.task_manager("team").unwrap().list().len(), 1);
    let rows = reopened
        .agent()
        .ctx()
        .session()
        .unwrap()
        .read_tasks(&reopened.context().id)
        .unwrap();
    assert_eq!(rows[0].subject, "new owner");
    assert_eq!(rows.len(), 1);
    reopened.close(None).await.unwrap();
}

#[tokio::test]
async fn task_tool_uses_the_shared_manager_and_both_hook_levels() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|_, _| {}));
    let mut tool_call = call("create-task", "tasks");
    tool_call.fn_arguments = json!({"action":"create","subject":"from tool"});
    let model = Arc::new(Model::new(vec![
        events(vec![end("", vec![tool_call])]),
        answer("done"),
    ]));
    let h = host(dir.path(), model, Some(hooks.clone())).await;
    let manager = h.task_manager("team").unwrap();
    let registry = Arc::new(ToolSet::default());
    registry.register(manager.clone());
    h.agent().ctx().set_tools(registry);
    h.submit(In::user_text("create a task")).unwrap();
    h.run_next(
        TurnLimits::default(),
        &DiscardSink,
        &CancellationToken::new(),
    )
    .await
    .unwrap();
    assert_eq!(manager.list()[0].subject, "from tool");
    let seen: Vec<_> = hooks
        .seen
        .lock()
        .unwrap()
        .iter()
        .copied()
        .filter(|kind| {
            matches!(
                kind,
                HookEventKind::PreToolUse
                    | HookEventKind::TaskCreated
                    | HookEventKind::PostToolUse
                    | HookEventKind::PostToolUseFailure
            )
        })
        .collect();
    assert_eq!(
        seen,
        [
            HookEventKind::PreToolUse,
            HookEventKind::TaskCreated,
            HookEventKind::PostToolUse
        ]
    );
    h.close(None).await.unwrap();
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
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
    let tool = subagent(&h, child_model, None);
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        cwd: None,
        call_id: "child-tool".into(),
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    let result = ToolProvider::run(tool.as_ref(), tc, json!({"prompt":"do work"}))
        .await
        .unwrap();
    assert_eq!(result["text"], "child result");
    // Completed children are released, not retained.
    assert_eq!(h.child_ids().len(), 0);
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
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
    recovered.close(Some(Duration::from_secs(1))).await.unwrap();
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
    let id = store
        .create_session(SessionId::new(), "instructions")
        .await
        .unwrap()
        .id;
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
    let budget = ModelBudget::new();
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
    // No shared call limit; a third call also succeeds.
    summary.complete(request).await.unwrap();
    let snapshot = budget.snapshot();
    assert_eq!(snapshot.calls, 3);
    assert_eq!(snapshot.usage.total_tokens, 29);
}
#[tokio::test]
async fn async_hook_completion_routes_to_own_session_and_wakes_host() {
    let dir = TempDir::new().unwrap();
    let runtime = Arc::new(yourai_harness::hooks::DefaultHookRuntime::new());
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
}
#[tokio::test]
async fn catalog_create_fork_list_delete_are_persistent() {
    let dir = TempDir::new().unwrap();
    let catalog = SessionCatalog::new(dir.path()).unwrap();
    let first = catalog.create_session(SessionId::new(), "").await.unwrap();
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
        h.close(Some(Duration::from_secs(1))),
        h.close(Some(Duration::from_secs(1)))
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
    next.close(Some(Duration::from_secs(1))).await.unwrap();
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
        .change_config("project", json!({"memory_search_limit":0}))
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
    use yourai_harness::hooks::{HookEvaluator, HookModelRequest};
    let budget = ModelBudget::new();
    let executor = assembly::model_hooks::DefaultHookEvaluator {
        model: Arc::new(MeteredModel {
            inner: Arc::new(Model::new(vec![
                answer(r#"{"ok":false,"reason":"unfinished"}"#),
                answer(r#"{"ok":true}"#),
            ])),
            budget: budget.clone(),
        }),
        tools: None,
        usage: None,
        timeout: Duration::from_secs(1),
        steps: 2,
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
    // No shared call limit; a second evaluation also succeeds.
    let result2 = executor.evaluate(request).await.unwrap();
    assert!(result2.ok);
    assert_eq!(budget.snapshot().calls, 2);
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
}

#[tokio::test]
async fn closing_session_cancels_its_background_command() {
    let dir = TempDir::new().unwrap();
    let marker = dir.path().join("should-not-exist");
    let runtime = Arc::new(yourai_harness::hooks::DefaultHookRuntime::new());
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
    tokio::time::sleep(Duration::from_millis(400)).await;
    assert!(!marker.exists());
}

#[tokio::test]
async fn shared_hook_runtime_keeps_background_contexts_session_local() {
    let dir = TempDir::new().unwrap();
    let runtime = Arc::new(yourai_harness::hooks::DefaultHookRuntime::new());
    let config:yourai_harness::hooks::HooksConfig=serde_json::from_value(json!({"hooks":{"SessionStart":[{"hooks":[{"type":"command","command":"sleep 0.1; echo feedback >&2; exit 2","asyncRewake":true}]}]}})).unwrap();
    runtime
        .register_config(&config, HookSource::Session)
        .await
        .unwrap();
    let a = Arc::new(Model::new(vec![answer("a")]));
    let b = Arc::new(Model::new(vec![answer("b")]));
    let first = yourai_harness::runtime::create(
        dir.path(),
        dir.path().into(),
        a.clone(),
        Some(runtime.clone()),
        None,
    )
    .await
    .unwrap();
    let second = yourai_harness::runtime::create(
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
        h.close(Some(Duration::from_secs(1))).await.unwrap();
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
    let id = store.create_session(SessionId::new(), "").await.unwrap().id;
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
    let id = store.create_session(SessionId::new(), "").await.unwrap().id;
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
    let session = dir
        .path()
        .join("sessions")
        .join(h.host.context().id.as_str());
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
    let id = store
        .create_session(SessionId::new(), &system)
        .await
        .unwrap()
        .id;
    let old = Arc::new(CountSummary(AtomicUsize::new(0)));
    let new = Arc::new(CountSummary(AtomicUsize::new(0)));
    let history = context(id.clone(), old.clone(), store.clone(), None);
    let old_hooks = Arc::new(Hooks::new(|_, _| {}));
    let new_hooks = Arc::new(Hooks::new(|_, _| {}));
    let agent = Agent::builder()
        .model(old.clone())
        .context_manager(history.inner.clone())
        .hooks(old_hooks.clone())
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
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
    let before = history
        .inner
        .build_request(&[], new.as_ref())
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
    h.close(Some(Duration::from_secs(1))).await.unwrap();
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
        fn classify_error(&self, _: &YourAiError) -> ModelErrorClass {
            ModelErrorClass::RateLimited
        }
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
    let budget = ModelBudget::new();
    let model = Arc::new(MeteredModel {
        inner: provider.clone(),
        budget: budget.clone(),
    });
    let agent = Agent::builder()
        .agent_loop(Arc::new(default_loop::DefaultLoop::default()))
        .model(model)
        .context_manager(Arc::new(History::default()))
        .build();
    let (events, result) = collect(agent.start(In::user_text("hello")).unwrap()).await;
    assert!(result.is_err());
    assert_eq!(provider.0.load(std::sync::atomic::Ordering::SeqCst), 6);
    let waits: Vec<_> = events
        .iter()
        .filter_map(|e| match e {
            Out::Retry { wait_ms, .. } => Some(*wait_ms),
            _ => None,
        })
        .collect();
    assert_eq!(waits.len(), 5);
    for (actual, base) in waits.iter().zip([2000, 4000, 8000, 16000, 30000]) {
        assert!(
            *actual >= base && *actual <= (base * 5 / 4).min(30000),
            "unexpected wait {actual}"
        );
    }
    let metrics = budget.snapshot();
    assert_eq!(metrics.calls, 6);
    assert_eq!(metrics.requests.rate_limited, 6);
    assert_eq!(metrics.requests.active, 0);
    assert_eq!(metrics.requests.cooldown_seconds, 0);
}

#[tokio::test]
async fn harness_model_switch_preserves_budget_history_and_updates_context() {
    let dir = TempDir::new().unwrap();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.context_policy.context_window = Some(64_000);
    let old = Arc::new(Model::new(vec![answer("first")]));
    let h = Harness::open(config, old.clone()).await.unwrap();
    h.host.submit(In::user_text("one")).unwrap();
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
    assert_eq!(h.budget.snapshot().calls, 1);

    let new = Arc::new(Model::new(vec![answer("second")]));
    let policy = ContextPolicy {
        context_window: Some(32_000),
        output_reserve: 2048,
        ..ContextPolicy::default()
    };
    h.switch_model_with_settings(
        new.clone(),
        policy,
        yourai_harness::assembly::ModelSettings {
            provider: "other".into(),
            requests: Default::default(),
            header_timeout: Some(Duration::from_secs(1)),
            chunk_timeout: Some(Duration::from_secs(1)),
        },
    )
    .await
    .unwrap();
    let usage = h.host.context_usage().unwrap();
    assert_eq!(usage.context_window, Some(32_000));
    assert_eq!(usage.output_reserve, 2048);
    assert_eq!(
        h.sessions
            .load_session(&h.host.context().id)
            .await
            .unwrap()
            .model
            .as_deref(),
        Some(new.model_iden())
    );
    h.host.submit(In::user_text("two")).unwrap();
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
    assert_eq!(h.budget.snapshot().calls, 2);
    assert_eq!(h.budget.snapshot().requests.completed, 2);
    assert_eq!(old.requests.lock().unwrap().len(), 1);
    assert!(new.requests.lock().unwrap()[0]
        .request
        .messages
        .iter()
        .any(|m| m.content.first_text() == Some("first")));
    h.close().await.unwrap();
}

#[tokio::test]
async fn harness_rejects_model_switch_during_a_turn_without_changing_context() {
    let dir = TempDir::new().unwrap();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.context_policy.context_window = Some(64_000);
    let old = Arc::new(Model::new(vec![Box::pin(futures_util::stream::pending())]));
    let h = Harness::open(config, old.clone()).await.unwrap();
    h.host.submit(In::user_text("wait")).unwrap();
    let host = h.host.clone();
    let turn = tokio::spawn(async move {
        host.run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        while old.requests.lock().unwrap().is_empty() {
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
    let policy = ContextPolicy {
        context_window: Some(32_000),
        ..ContextPolicy::default()
    };
    let new = Arc::new(Model::new(vec![answer("new")]));
    assert!(h
        .switch_model(new.clone(), policy)
        .await
        .unwrap_err()
        .to_string()
        .contains("busy"));
    assert_eq!(h.host.context_usage().unwrap().context_window, Some(64_000));
    assert!(new.requests.lock().unwrap().is_empty());
    h.host.interrupt();
    tokio::time::timeout(Duration::from_secs(5), turn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    h.close().await.unwrap();
}

// Real filesystem workers and a paused Tokio clock do not share progress:
// auto-advance can expire the outer guard while fsync is still running.
#[tokio::test]
async fn model_switch_publishes_chunk_timeout_with_model() {
    let dir = TempDir::new().unwrap();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    let h = Harness::open(config, Arc::new(Model::new(vec![answer("old")])))
        .await
        .unwrap();
    let next = Arc::new(Model::new(vec![hangs_after("partial")]));
    h.switch_model_with_settings(
        next,
        ContextPolicy::default(),
        yourai_harness::assembly::ModelSettings {
            provider: "new".into(),
            requests: Default::default(),
            header_timeout: Some(Duration::from_millis(20)),
            chunk_timeout: Some(Duration::from_millis(20)),
        },
    )
    .await
    .unwrap();
    h.host.submit(In::user_text("hello")).unwrap();
    let report = tokio::time::timeout(
        Duration::from_secs(1),
        h.host.run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        ),
    )
    .await
    .expect("new timeout must replace the old 300-second default")
    .unwrap()
    .unwrap();
    assert!(report.result.is_err());
    h.close().await.unwrap();
}

#[tokio::test(start_paused = true)]
async fn close_can_wait_for_cleanup_without_an_implicit_deadline() {
    struct SlowEnd;
    impl HookRuntime for SlowEnd {
        fn dispatch<'a>(
            &'a self,
            i: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
            Box::pin(async move {
                if matches!(i.event, HookEvent::SessionEnd { .. }) {
                    tokio::time::sleep(Duration::from_secs(20)).await;
                }
                Ok(HookDispatchResult::empty(i.event.kind()))
            })
        }
    }
    let dir = TempDir::new().unwrap();
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(Arc::new(SlowEnd)),
    )
    .await;
    let start = tokio::time::Instant::now();
    h.close(None).await.unwrap();
    assert!(start.elapsed() >= Duration::from_secs(20));
    assert_eq!(h.status(), SessionStatus::Closed);
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn harness_model_settings_are_inherited_by_child_default_loop() {
    let dir = TempDir::new().unwrap();
    let mut config = HarnessConfig::new(dir.path().join("sessions"), dir.path().into());
    config.system_prompt = Some("test".into());
    config.extensions = true;
    config.yolo = true;
    config.model_header_timeout = Some(Duration::from_secs(7));
    config.model_chunk_timeout = Some(Duration::from_secs(11));
    let mut spawn = call("spawn", "subagent");
    spawn.fn_arguments = json!({"prompt":"child task"});
    let model = Arc::new(Model::new(vec![
        events(vec![end("", vec![spawn])]),
        answer("child done"),
        answer("parent done"),
    ]));
    let h = Harness::open(config, model.clone()).await.unwrap();
    h.host.submit_async(In::user_text("go")).await.unwrap();
    let result = h
        .host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert_eq!(result.result.unwrap().text, "parent done");
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 3);
        assert_ne!(requests[0].session_id, requests[1].session_id);
        for request in requests.iter() {
            assert_eq!(
                request.options.stream_header_timeout,
                Some(Duration::from_secs(7))
            );
            assert_eq!(
                request.options.stream_read_timeout,
                Some(Duration::from_secs(11))
            );
        }
    }
    h.close().await.unwrap();
}

#[tokio::test]
async fn host_rejects_matching_event_with_wrong_hook_outcome() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("db.sqlite3")).unwrap());
    let id = store.create_session(SessionId::new(), "").await.unwrap().id;
    let model = Arc::new(Model::new(vec![]));
    let hooks = Arc::new(Hooks::new(|_, result| {
        result.outcome = HookPointOutcome::PreToolUse(Default::default());
    }));
    let history = context(id.clone(), model.clone(), store.clone(), None);
    let agent = Agent::builder()
        .agent_loop(Arc::new(default_loop::DefaultLoop::default()))
        .model(model)
        .context_manager(history)
        .session(store)
        .hooks(hooks)
        .build();
    let result = SessionHost::open(
        dir.path(),
        SessionContext::new(id, dir.path()),
        agent,
        HostConfig::default(),
        "startup",
    )
    .await;
    assert!(result
        .err()
        .expect("wrong outcome must fail host startup")
        .to_string()
        .contains("mismatched"));
}

#[tokio::test]
async fn blocked_subagent_start_does_not_create_a_persistent_session() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|i, result| {
        if matches!(i.event, HookEvent::SubagentStart { .. }) {
            result.common.blocking_errors.push(HookBlockingError {
                hook_id: "veto-child".into(),
                message: "blocked".into(),
            });
        }
    }));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let store = SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap();
    let before = store.list_sessions().await.unwrap().len();
    let tool = subagent(&h, Arc::new(Model::new(vec![])), None);
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        cwd: None,
        call_id: "blocked-child".into(),
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    assert!(
        ToolProvider::run(tool.as_ref(), tc, json!({"prompt": "do work"}))
            .await
            .is_err()
    );
    assert_eq!(store.list_sessions().await.unwrap().len(), before);
    assert!(!dir.path().join("children").exists());
    assert!(h.child_ids().is_empty());
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::SubagentStop));
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn subagent_hook_identity_matches_persisted_session_and_continues_same_child() {
    let dir = TempDir::new().unwrap();
    let ids = Arc::new(std::sync::Mutex::new(Vec::new()));
    let recorded = ids.clone();
    let hooks = Arc::new(Hooks::new(move |i, result| match &i.event {
        HookEvent::SubagentStart { agent_id, .. } => {
            recorded.lock().unwrap().push(agent_id.clone())
        }
        HookEvent::SubagentStop {
            agent_id,
            stop_hook_active,
            ..
        } => {
            recorded.lock().unwrap().push(agent_id.clone());
            if !stop_hook_active {
                result.common.blocking_errors.push(HookBlockingError {
                    hook_id: "continue".into(),
                    message: "finish the remaining work".into(),
                });
            }
        }
        _ => {}
    }));
    let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
    let tool = subagent(
        &h,
        Arc::new(Model::new(vec![answer("first"), answer("second")])),
        None,
    );
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        cwd: None,
        call_id: "child".into(),
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    let result = ToolProvider::run(tool.as_ref(), tc, json!({"prompt": "do work"}))
        .await
        .unwrap();
    assert_eq!(result["text"], "second");
    let id = result["agent_id"].as_str().unwrap();
    assert_eq!(*ids.lock().unwrap(), vec![id, id, id]);
    let store = SqliteStore::open(&SqliteStore::path(&dir.path().join("children"))).unwrap();
    let child = store.load_session(&SessionId::from(id)).await.unwrap();
    assert_eq!(child.parent_session_id, Some(h.context().id));
    assert!(h.child_ids().is_empty());
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn dynamic_instructions_reach_the_next_request_and_do_not_duplicate() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("DYNAMIC.md");
    std::fs::write(&path, "DYNAMIC-INSTRUCTION-CONTENT").unwrap();
    let model = Arc::new(Model::new(vec![answer("done")]));
    let h = host(dir.path(), model.clone(), None).await;
    let workspace = h.workspace().unwrap();
    assert!(Arc::ptr_eq(&workspace, &h.workspace().unwrap()));
    workspace.load_instructions(&path, "include").await.unwrap();
    workspace.load_instructions(&path, "include").await.unwrap();
    h.submit(In::user_text("continue")).unwrap();
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
    let request = model.requests.lock().unwrap()[0].request.clone();
    assert_eq!(
        request
            .messages
            .iter()
            .filter(|m| m
                .content
                .first_text()
                .is_some_and(|s| s.contains("DYNAMIC-INSTRUCTION-CONTENT")))
            .count(),
        1
    );
    assert!(request
        .system
        .as_ref()
        .is_none_or(|s| !s.contains("DYNAMIC-INSTRUCTION-CONTENT")));
    h.close(None).await.unwrap();
}

#[derive(Default)]
struct LifecycleHooks {
    fail_start: bool,
    pause_start: bool,
    start_release: tokio::sync::Notify,
    fail_shutdown: bool,
    entered: tokio::sync::Notify,
    cleaned: tokio::sync::Notify,
    shutdowns: std::sync::atomic::AtomicUsize,
    ends: std::sync::atomic::AtomicUsize,
}
impl HookRuntime for LifecycleHooks {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            if invocation.event.kind() == HookEventKind::SessionStart {
                self.entered.notify_one();
                if self.pause_start {
                    self.start_release.notified().await;
                }
                if self.fail_start {
                    return Err(ErrorKind::Config("startup failed".into()).into());
                }
            }
            if invocation.event.kind() == HookEventKind::SessionEnd {
                self.ends.fetch_add(1, Ordering::SeqCst);
            }
            Ok(HookDispatchResult::empty(invocation.event.kind()))
        })
    }
    fn shutdown_session<'a>(&'a self, _: &'a str) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.shutdowns.fetch_add(1, Ordering::SeqCst);
            self.cleaned.notify_one();
            if self.fail_shutdown {
                Err(ErrorKind::Config("hook shutdown failed".into()).into())
            } else {
                Ok(())
            }
        })
    }
}
fn startup_agent(id: SessionId, hooks: Arc<dyn HookRuntime>) -> Arc<Agent> {
    Agent::builder()
        .context_manager(yourai_harness::DefaultContext::memory(id))
        .hooks(hooks)
        .build()
}
#[tokio::test]
async fn failed_startup_reclaims_hook_resources_and_releases_the_lease() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(LifecycleHooks {
        fail_start: true,
        ..Default::default()
    });
    let id = SessionId::new();
    let result = SessionHost::open(
        dir.path(),
        SessionContext::new(id.clone(), dir.path()),
        startup_agent(id, hooks.clone()),
        HostConfig::default(),
        "startup",
    )
    .await;
    assert!(result.is_err());
    assert_eq!(hooks.shutdowns.load(Ordering::SeqCst), 1);
    assert_eq!(hooks.ends.load(Ordering::SeqCst), 0);
    assert!(SessionLease::acquire(dir.path().into()).is_ok());
}
#[tokio::test]
async fn abandoned_startup_keeps_an_owner_until_cleanup_finishes() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(LifecycleHooks {
        pause_start: true,
        ..Default::default()
    });
    let id = SessionId::new();
    let mut opening = Box::pin(SessionHost::open(
        dir.path(),
        SessionContext::new(id.clone(), dir.path()),
        startup_agent(id, hooks.clone()),
        HostConfig::default(),
        "startup",
    ));
    tokio::select! { _ = hooks.entered.notified() => {}, _ = &mut opening => panic!("startup must remain pending") }
    drop(opening);
    tokio::time::timeout(Duration::from_secs(2), hooks.cleaned.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if SessionLease::acquire(dir.path().into()).is_ok() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(hooks.shutdowns.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn hook_shutdown_failure_cannot_swallow_inputs_or_keep_the_lease() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(LifecycleHooks {
        fail_shutdown: true,
        ..Default::default()
    });
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    h.submit(In::follow_up("handoff")).unwrap();
    assert_eq!(h.close(None).await.unwrap().len(), 1);
    assert_eq!(h.status(), SessionStatus::Closed);
    assert!(h.last_error().unwrap().contains("hook shutdown failed"));
    assert!(h.close(None).await.unwrap().is_empty());
    assert_eq!(hooks.ends.load(Ordering::SeqCst), 1);
    assert!(SessionLease::acquire(dir.path().into()).is_ok());
}
#[tokio::test]
async fn retrying_close_after_journal_failure_does_not_repeat_confirmed_session_end() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(LifecycleHooks::default());
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    h.submit(In::follow_up("handoff")).unwrap();
    let journal = dir.path().join("host.json");
    std::fs::remove_file(&journal).unwrap();
    std::fs::create_dir(&journal).unwrap();
    assert!(h.close(None).await.is_err());
    assert_eq!(h.status(), SessionStatus::Closing);
    std::fs::remove_dir(&journal).unwrap();
    assert_eq!(h.close(None).await.unwrap().len(), 1);
    assert_eq!(hooks.ends.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn subagent_start_context_goes_to_the_child_request() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(Hooks::new(|invocation, result| {
        if invocation.event.kind() == HookEventKind::SubagentStart {
            result.outcome = HookPointOutcome::Generic(GenericOutcome {
                additional_contexts: vec!["CHILD-START-INSTRUCTION".into()],
            });
        }
    }));
    let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
    let child_model = Arc::new(Model::new(vec![answer("child done")]));
    let tool = subagent(&h, child_model.clone(), None);
    let cancel = CancellationToken::new();
    let tc = ToolContext {
        cwd: None,
        call_id: "child-context".into(),
        emit: &DiscardSink,
        cancel: &cancel,
        security: None,
        sandbox: None,
        interaction: None,
    };
    ToolProvider::run(tool.as_ref(), tc, json!({"prompt":"work"}))
        .await
        .unwrap();
    assert!(
        serde_json::to_string(&child_model.requests.lock().unwrap()[0].request)
            .unwrap()
            .contains("CHILD-START-INSTRUCTION")
    );
    assert!(!h
        .agent()
        .ctx()
        .context_manager()
        .unwrap()
        .messages()
        .iter()
        .any(|m| m
            .content
            .first_text()
            .is_some_and(|s| s.contains("CHILD-START-INSTRUCTION"))));
    assert!(h.child_ids().is_empty());
    h.close(None).await.unwrap();
}
struct PauseConfigHook(tokio::sync::Notify);
impl HookRuntime for PauseConfigHook {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            if invocation.event.kind() == HookEventKind::ConfigChange {
                self.0.notify_one();
                std::future::pending::<()>().await;
            }
            Ok(HookDispatchResult::empty(invocation.event.kind()))
        })
    }
}
#[tokio::test]
async fn dropped_config_change_removes_its_candidate() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(PauseConfigHook(tokio::sync::Notify::new()));
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(hooks.clone()),
    )
    .await;
    let workspace = h.workspace().unwrap();
    let worker = tokio::spawn(async move {
        workspace
            .change_config("project", json!({"memory_search_limit":3}))
            .await
    });
    hooks.0.notified().await;
    let candidate = dir.path().join("config.candidate.json");
    assert!(candidate.exists());
    worker.abort();
    assert!(worker.await.unwrap_err().is_cancelled());
    assert!(!candidate.exists());
    assert_eq!(h.workspace().unwrap().config().unwrap(), json!({}));
    h.close(None).await.unwrap();
}
fn initialize_repo(path: &std::path::Path) {
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
            .current_dir(path)
            .output()
            .unwrap()
            .status
            .success());
    }
}
#[tokio::test]
async fn worktree_registration_failures_report_effects_and_retries_adopt_them() {
    let repo = TempDir::new().unwrap();
    initialize_repo(repo.path());
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let workspace = h.workspace().unwrap();
    workspace.change_cwd(repo.path()).await.unwrap();
    let registration = dir.path().join("worktrees.json");
    std::fs::create_dir(&registration).unwrap();
    let failure = workspace.create_worktree("retry").await.unwrap_err();
    assert!(failure.to_string().contains("git worktree exists at"));
    let path = dir.path().join("worktrees/retry");
    assert!(path.join(".git").is_file());
    std::fs::remove_dir(&registration).unwrap();
    assert_eq!(
        workspace.create_worktree("retry").await.unwrap(),
        path.canonicalize().unwrap()
    );
    std::fs::remove_file(&registration).unwrap();
    std::fs::create_dir(&registration).unwrap();
    let failure = workspace.remove_worktree("retry").await.unwrap_err();
    assert!(failure.to_string().contains("git worktree removed at"));
    assert!(!path.exists());
    std::fs::remove_dir(&registration).unwrap();
    workspace.remove_worktree("retry").await.unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&std::fs::read(registration).unwrap()).unwrap(),
        json!({})
    );
    h.close(None).await.unwrap();
}
#[tokio::test]
async fn hook_worktree_paths_must_be_git_worktrees_in_the_same_repository() {
    let repo = TempDir::new().unwrap();
    initialize_repo(repo.path());
    let other = TempDir::new().unwrap();
    initialize_repo(other.path());
    let linked = other.path().join("linked");
    assert!(std::process::Command::new("git")
        .args([
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD"
        ])
        .current_dir(other.path())
        .output()
        .unwrap()
        .status
        .success());
    for candidate in [repo.path().to_path_buf(), linked] {
        let dir = TempDir::new().unwrap();
        let hook_path = candidate.to_string_lossy().into_owned();
        let hooks = Arc::new(Hooks::new(move |invocation, result| {
            if invocation.event.kind() == HookEventKind::WorktreeCreate {
                result.outcome =
                    HookPointOutcome::WorktreeCreate(yourai_core::hooks::WorktreeCreateOutcome {
                        worktree_path: Some(hook_path.clone()),
                    });
            }
        }));
        let h = host(dir.path(), Arc::new(Model::new(vec![])), Some(hooks)).await;
        let workspace = h.workspace().unwrap();
        workspace.change_cwd(repo.path()).await.unwrap();
        assert!(workspace.create_worktree("invalid").await.is_err());
        assert!(!dir.path().join("worktrees.json").exists());
        h.close(None).await.unwrap();
    }
}

struct ChildProgress(Arc<tokio::sync::Notify>);
impl OutSink for ChildProgress {
    fn send(&self, _: Out) -> bool {
        self.0.notify_one();
        true
    }
}
#[tokio::test]
async fn parent_close_cancels_direct_subagent_and_reclaims_the_unique_child() {
    let dir = TempDir::new().unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let provider = subagent(
        &h,
        Arc::new(Model::new(vec![hangs_after("child partial")])),
        None,
    );
    let entered = Arc::new(tokio::sync::Notify::new());
    let signal = entered.clone();
    let worker = tokio::spawn(async move {
        let cancel = CancellationToken::new();
        let sink = ChildProgress(signal);
        let tc = ToolContext {
            cwd: None,
            call_id: "direct-child".into(),
            emit: &sink,
            cancel: &cancel,
            security: None,
            sandbox: None,
            interaction: None,
        };
        ToolProvider::run(provider.as_ref(), tc, json!({"prompt":"work"})).await
    });
    tokio::time::timeout(Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    assert_eq!(h.child_ids().len(), 1);
    h.close(Some(Duration::from_secs(2))).await.unwrap();
    assert!(worker.await.unwrap().is_err());
    assert!(h.child_ids().is_empty());
    assert_eq!(h.status(), SessionStatus::Closed);
}
struct FileHookFailure;
impl HookRuntime for FileHookFailure {
    fn dispatch<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
        Box::pin(async move {
            if invocation.event.kind() == HookEventKind::FileChanged {
                return Err(ErrorKind::Config("file observer unavailable".into()).into());
            }
            Ok(HookDispatchResult::empty(invocation.event.kind()))
        })
    }
}
#[tokio::test]
async fn file_changed_hook_failure_is_visible_and_does_not_lose_the_fact() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("observed.txt");
    std::fs::write(&path, "old").unwrap();
    let h = host(
        dir.path(),
        Arc::new(Model::new(vec![])),
        Some(Arc::new(FileHookFailure)),
    )
    .await;
    h.watch_path(path.clone()).unwrap();
    std::fs::write(&path, "new contents").unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let journal: serde_json::Value =
                serde_json::from_slice(&std::fs::read(dir.path().join("host.json")).unwrap())
                    .unwrap();
            if journal["events"].as_array().unwrap().iter().any(|event| {
                event["notice"]
                    .as_str()
                    .is_some_and(|s| s.contains("FileChanged hook failed"))
                    && event["context"]
                        .as_str()
                        .is_some_and(|s| s.contains("File modified:") && s.contains("observed.txt"))
            }) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "new contents");
    h.close(None).await.unwrap();
}

#[tokio::test]
async fn abandoned_completed_startup_also_reclaims_unpublished_resources() {
    let dir = TempDir::new().unwrap();
    let hooks = Arc::new(LifecycleHooks {
        pause_start: true,
        ..Default::default()
    });
    let id = SessionId::new();
    let mut opening = Box::pin(SessionHost::open(
        dir.path(),
        SessionContext::new(id.clone(), dir.path()),
        startup_agent(id, hooks.clone()),
        HostConfig::default(),
        "startup",
    ));
    tokio::select! { _ = hooks.entered.notified() => {}, _ = &mut opening => panic!("startup must remain pending") }
    hooks.start_release.notify_one();
    // Leave the completed result in its channel without polling the caller.
    tokio::time::sleep(Duration::from_millis(10)).await;
    assert!(SessionLease::acquire(dir.path().into()).is_err());
    drop(opening);
    tokio::time::timeout(Duration::from_secs(2), hooks.cleaned.notified())
        .await
        .unwrap();
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            if SessionLease::acquire(dir.path().into()).is_ok() {
                break;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn legacy_runtime_prompt_setting_is_removed_without_changing_frozen_prompt() {
    let dir = TempDir::new().unwrap();
    std::fs::write(
        dir.path().join("config.json"),
        r#"{"system_prompt":"ignored legacy value","memory_search_limit":7}"#,
    )
    .unwrap();
    let h = host(dir.path(), Arc::new(Model::new(vec![])), None).await;
    let config = h.workspace().unwrap().config().unwrap();
    assert_eq!(config["memory_search_limit"], 7);
    assert!(config.get("system_prompt").is_none());
    assert_eq!(
        h.agent().ctx().context_manager().unwrap().system_prompt(),
        ""
    );
    h.close(None).await.unwrap();
}

#[derive(Default)]
struct SuspendedRestore {
    inner: History,
    suspended: AtomicBool,
    entered: tokio::sync::Notify,
    released: tokio::sync::Notify,
}
impl SuspendedRestore {
    fn resume(&self) {
        self.suspended.store(false, Ordering::SeqCst);
        self.released.notify_waiters();
    }
}
impl ContextManager for SuspendedRestore {
    fn system_prompt(&self) -> String {
        self.inner.system_prompt()
    }
    fn session_id(&self) -> &SessionId {
        self.inner.session_id()
    }
    fn restore(&self) -> BoxFuture<'_, Result<(), YourAiError>> {
        Box::pin(async move {
            while self.suspended.load(Ordering::SeqCst) {
                let released = self.released.notified();
                self.entered.notify_one();
                released.await;
            }
            self.inner.restore().await
        })
    }
    fn append(&self, messages: Vec<StoredMessage>) -> BoxFuture<'_, Result<(), YourAiError>> {
        self.inner.append(messages)
    }
    fn build_request(
        &self,
        tools: &[ToolDefinition],
        model: &dyn ModelProvider,
    ) -> Result<ContextRequest, YourAiError> {
        self.inner.build_request(tools, model)
    }
    fn records(&self) -> Vec<StoredMessage> {
        self.inner.records()
    }
    fn prepare_compaction<'a>(
        &'a self,
        request: &'a CompactionRequest,
        model: &'a dyn ModelProvider,
        cancel: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<CompactionPlan<'a>, YourAiError>> {
        self.inner.prepare_compaction(request, model, cancel)
    }
}
async fn host_with_suspended_restore(
    dir: &std::path::Path,
) -> (Arc<SessionHost>, Arc<SuspendedRestore>, Arc<Model>) {
    let history = Arc::new(SuspendedRestore::default());
    let model = Arc::new(Model::new(vec![answer("done")]));
    let agent = Agent::builder()
        .agent_loop(Arc::new(default_loop::DefaultLoop::default()))
        .model(model.clone())
        .context_manager(history.clone())
        .build();
    let host = SessionHost::open(
        dir,
        SessionContext::new(history.session_id().clone(), dir),
        agent,
        HostConfig::default(),
        "startup",
    )
    .await
    .unwrap();
    history.suspended.store(true, Ordering::SeqCst);
    (host, history, model)
}
#[tokio::test]
async fn cancelled_host_restore_preserves_the_unstarted_input() {
    let dir = TempDir::new().unwrap();
    let (host, history, model) = host_with_suspended_restore(dir.path()).await;
    host.submit(In::user_text("queued before restore")).unwrap();
    let cancel = CancellationToken::new();
    let driver = {
        let host = host.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            host.run_next(TurnLimits::default(), &DiscardSink, &cancel)
                .await
        })
    };
    history.entered.notified().await;
    cancel.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(1), driver)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        failure,
        YourAiError::Aborted(AbortReason::Cancelled)
    ));
    assert_eq!(host.queued(), 1);
    assert_eq!(host.status(), SessionStatus::Idle);
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(history.records().is_empty());
    history.resume();
    let report = host
        .run_next(
            TurnLimits::default(),
            &DiscardSink,
            &CancellationToken::new(),
        )
        .await
        .unwrap()
        .unwrap();
    assert!(report.result.is_ok());
    assert_eq!(host.queued(), 0);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    host.close(None).await.unwrap();
}
#[tokio::test]
async fn cancelled_manual_compaction_restore_releases_its_gate() {
    let dir = TempDir::new().unwrap();
    let (host, history, model) = host_with_suspended_restore(dir.path()).await;
    let cancel = CancellationToken::new();
    let compacting = {
        let host = host.clone();
        let cancel = cancel.clone();
        tokio::spawn(async move {
            host.compact(CompactionRequest::new(CompactionTrigger::Manual), &cancel)
                .await
        })
    };
    history.entered.notified().await;
    cancel.cancel();
    let failure = tokio::time::timeout(Duration::from_secs(1), compacting)
        .await
        .unwrap()
        .unwrap()
        .unwrap_err();
    assert!(matches!(
        failure,
        YourAiError::Aborted(AbortReason::Cancelled)
    ));
    assert_eq!(host.status(), SessionStatus::Idle);
    assert!(history.inner.compactions.lock().unwrap().is_empty());
    assert!(model.requests.lock().unwrap().is_empty());
    history.resume();
    host.close(None).await.unwrap();
}
