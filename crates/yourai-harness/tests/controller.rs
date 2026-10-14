//! Backend behavior must not require a frontend to poll or consume output.
use std::{sync::Arc, time::Duration};
use yourai_core::prelude::*;
use yourai_harness::{
    config::Config,
    controller::{Reply, SessionController, SessionEvent, SESSION_EVENT_CAPACITY},
    Harness, SessionOptions,
};
struct Program;
impl AgentLoop for Program {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut turn = Turn::open(tc).await?;
            let result = turn.complete("backend completed").await.map(|_| ());
            turn.finish(result).await
        })
    }
}
async fn fixture() -> (tempfile::TempDir, Harness, Arc<Config>, SessionOptions) {
    fixture_with_loop(Arc::new(Program)).await
}
async fn fixture_with_loop(
    agent_loop: Arc<dyn AgentLoop>,
) -> (tempfile::TempDir, Harness, Arc<Config>, SessionOptions) {
    let dir = tempfile::tempdir().unwrap();
    let config: Config = serde_json::from_value(serde_json::json!({
      "models": {
        "model": "mock/test",
        "providers": {
          "mock": {
            "options": {
              "baseURL": "http://127.0.0.1:1/v1",
              "apiKey": "test"
            },
            "models": {}
          }
        }
      }
    }))
    .unwrap();
    let path = dir.path().join("yourai.json");
    std::fs::write(&path, serde_json::to_vec(&config).unwrap()).unwrap();
    let config = Config::init(&path).unwrap();
    let model = yourai_harness::model::resolve(&config).unwrap().model;
    let mut options = SessionOptions::new(dir.path().join("sessions"), dir.path().into());
    options.config.set_system_prompt(Some("test".into()));
    options.agent_loop = Some(agent_loop);
    let h = Harness::open(options.clone(), model).await.unwrap();
    (dir, h, Arc::new(config), options)
}
async fn wait_until(mut ready: impl FnMut() -> bool) {
    tokio::time::timeout(Duration::from_secs(10), async {
        while !ready() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
}
#[tokio::test]
async fn execution_and_operation_completion_progress_without_frontend_polling() {
    let (_dir, h, config, options) = fixture().await;
    let (backend, mut events) = SessionController::new(h, config, options, TurnLimits::default());
    assert!(backend
        .client()
        .submit(In::user_text("execute without a UI")));
    wait_until(|| {
        backend
            .harness()
            .host
            .agent()
            .context_manager()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.content.first_text() == Some("backend completed"))
    })
    .await;
    wait_until(|| backend.client().idle()).await;
    backend.client().list().unwrap();
    assert!(!backend
        .client()
        .submit(In::user_text("defer during operation")));
    assert!(matches!(
        backend
            .client()
            .reply("unknown".into(), serde_json::json!({})),
        Reply::Deferred
    ));
    assert!(backend
        .client()
        .model("other/model".into(), None, None)
        .is_err());
    wait_until(|| !backend.client().busy()).await;
    let mut listed = false;
    while let Some(event) = events.try_recv() {
        if let SessionEvent::Listed(Ok(rows)) = event {
            assert_eq!(rows.len(), 1);
            listed = true;
        }
    }
    assert!(listed);
    assert!(matches!(
        backend
            .client()
            .reply("unknown".into(), serde_json::json!({})),
        Reply::Rejected(_)
    ));
    backend.close().await.unwrap();
}
struct CloseGate {
    entered: tokio::sync::Notify,
    release: tokio::sync::Notify,
}
impl HookHandler for CloseGate {
    fn execute<'a>(
        &'a self,
        _: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        Box::pin(async move {
            self.entered.notify_one();
            self.release.notified().await;
            Ok(HookOutput::Parsed(serde_json::json!({})))
        })
    }
}
#[tokio::test]
async fn new_session_accepts_input_while_backend_retires_the_previous_session() {
    let (_dir, h, config, options) = fixture().await;
    let old = h.host.clone();
    let id = old.environment().id;
    let gate = Arc::new(CloseGate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Notify::new(),
    });
    h.hooks
        .register(NativeHookRegistration {
            id: "close-gate".into(),
            event: HookEventKind::SessionEnd,
            matcher: None,
            handler: gate.clone(),
            timeout: Some(Duration::from_secs(15)),
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: true,
        })
        .await
        .unwrap();
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    backend.client().switch(None).unwrap();
    tokio::time::timeout(Duration::from_secs(10), gate.entered.notified())
        .await
        .unwrap();
    assert_ne!(backend.client().environment().id, id);
    assert!(backend.client().busy());
    assert!(backend.client().list().is_err());
    assert!(backend.client().submit(In::user_text("new session input")));
    wait_until(|| {
        backend
            .harness()
            .host
            .agent()
            .context_manager()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.content.first_text() == Some("backend completed"))
    })
    .await;
    gate.release.notify_one();
    wait_until(|| !backend.client().busy()).await;
    assert_eq!(old.status(), SessionStatus::Closed);
    backend.close().await.unwrap();
}
#[tokio::test]
async fn failed_switch_keeps_the_current_backend_and_current_session_cannot_be_deleted() {
    let (_dir, h, config, options) = fixture().await;
    let (backend, mut events) = SessionController::new(h, config, options, TurnLimits::default());
    let id = backend.client().environment().id;
    assert!(backend.client().delete(id.clone()).is_err());
    backend.client().switch(Some(SessionId::new())).unwrap();
    wait_until(|| !backend.client().busy()).await;
    assert_eq!(backend.client().environment().id, id);
    assert!(std::iter::from_fn(|| events.try_recv())
        .any(|event| matches!(event, SessionEvent::Opened(Err(_)))));
    backend.close().await.unwrap();
}
#[tokio::test]
async fn task_queries_expose_one_versioned_snapshot_and_queued_input_blocks_switching() {
    let (_dir, h, config, options) = fixture().await;
    let task = h
        .tasks
        .as_ref()
        .unwrap()
        .create("one".into(), None, None)
        .await
        .unwrap();
    let snapshot = h.host.tasks_since(None).unwrap();
    assert_eq!(snapshot.1[0].id, task.id);
    assert!(h.host.tasks_since(Some(snapshot.0)).is_none());
    h.host.submit(In::follow_up("queued")).unwrap();
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    assert!(backend.client().switch(None).is_err());
    backend.close().await.unwrap();
}

#[tokio::test]
async fn disconnecting_clients_keeps_execution_alive_and_close_rejects_live_clients() {
    let (_dir, h, config, options) = fixture().await;
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    let h = backend.harness();
    let client = backend.client();
    assert!(client.submit(In::user_text("finish after disconnect")));
    drop(client);
    wait_until(|| {
        h.host
            .agent()
            .context_manager()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.content.first_text() == Some("backend completed"))
    })
    .await;
    wait_until(|| backend.client().idle()).await;
    let client = backend.client();
    tokio::time::timeout(Duration::from_secs(10), backend.close())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(h.host.status(), SessionStatus::Closed));
    assert!(!client.submit(In::user_text("after close")));
    assert!(matches!(
        client.reply("unknown".into(), serde_json::json!({})),
        Reply::Rejected(_)
    ));
    assert!(client.list().is_err());
    assert!(client.steer_pending("unknown".into()).is_err());
    assert!(client.change_yolo(true).is_err());
}

#[tokio::test]
async fn dropping_backend_owner_closes_session_even_with_connected_clients() {
    let (_dir, h, config, options) = fixture().await;
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    let h = backend.harness();
    let client = backend.client();
    drop(backend);
    assert!(!client.submit(In::user_text("owner gone")));
    wait_until(|| matches!(h.host.status(), SessionStatus::Closed)).await;
}

struct BurstProgram;
impl AgentLoop for BurstProgram {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            for i in 0..SESSION_EVENT_CAPACITY * 3 {
                assert!(tc.outbox.send(Out::Notice {
                    level: Level::Info,
                    message: format!("burst:{i}"),
                }));
            }
            Program.run_turn(tc).await
        })
    }
}

#[tokio::test]
async fn independent_subscribers_report_gaps_without_blocking_execution() {
    let (_dir, h, config, options) = fixture_with_loop(Arc::new(BurstProgram)).await;
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    let client = backend.client();
    let mut slow = client.subscribe().unwrap();
    let mut fast = client.subscribe().unwrap();
    let (published, published_rx) = tokio::sync::oneshot::channel();
    let reader = tokio::spawn(async move {
        let mut published = Some(published);
        let mut notices = Vec::new();
        loop {
            match fast.recv().await {
                Some(SessionEvent::Output(Out::Notice { message, .. }))
                    if message.starts_with("burst:") =>
                {
                    notices.push(message);
                    if notices.len() == SESSION_EVENT_CAPACITY * 3 {
                        let _ = published.take().unwrap().send(());
                    }
                }
                Some(SessionEvent::Lagged { skipped }) => panic!("fast subscriber lost {skipped}"),
                Some(_) => {}
                None => break,
            }
        }
        notices
    });
    assert!(client.submit(In::user_text("broadcast without a frontend poll")));
    wait_until(|| {
        backend
            .harness()
            .host
            .agent()
            .context_manager()
            .unwrap()
            .messages()
            .iter()
            .any(|m| m.content.first_text() == Some("backend completed"))
    })
    .await;
    wait_until(|| backend.client().idle()).await;
    // Let the backend publish the final buffered output before stopping it.
    tokio::time::timeout(Duration::from_secs(10), published_rx)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(slow.try_recv(), Some(SessionEvent::Lagged { skipped }) if skipped > 0));
    assert!(slow.has_events());
    drop(slow); // Unsubscribing one reader does not affect another reader or execution.
    backend.close().await.unwrap();
    let notices = tokio::time::timeout(Duration::from_secs(10), reader)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(notices.len(), SESSION_EVENT_CAPACITY * 3);
    for (i, message) in notices.iter().enumerate() {
        assert_eq!(message, &format!("burst:{i}"));
    }
    assert!(client.subscribe().is_err());
}

#[tokio::test]
async fn subscribing_starts_at_future_events_and_close_ends_existing_subscriptions() {
    let (_dir, h, config, options) = fixture().await;
    let (backend, _events) = SessionController::new(h, config, options, TurnLimits::default());
    backend.client().list().unwrap();
    wait_until(|| !backend.client().busy()).await;
    let client = backend.client();
    let mut first = client.subscribe().unwrap();
    let mut second = client.subscribe().unwrap();
    assert!(first.try_recv().is_none()); // No implicit replay of the previous list result.
    backend.client().list().unwrap();
    for subscription in [&mut first, &mut second] {
        let event = tokio::time::timeout(Duration::from_secs(10), subscription.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(event, SessionEvent::Listed(Ok(rows)) if rows.len() == 1));
    }
    drop(first);
    backend.close().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), async {
        while second.recv().await.is_some() {}
    })
    .await
    .unwrap();
}
