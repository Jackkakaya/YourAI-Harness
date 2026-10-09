use super::test_support::{fixture_with_hooks, Hooks};
use super::*;
use crate::hooks::SessionStartOutcome;
use std::sync::atomic::Ordering;
fn start_result(paths: Vec<&str>, initial: Option<&str>) -> HookDispatchResult {
    let mut result = HookDispatchResult::empty(HookEventKind::SessionStart);
    result.outcome = HookPointOutcome::SessionStart(SessionStartOutcome {
        initial_user_message: initial.map(String::from),
        watch_paths: paths.into_iter().map(String::from).collect(),
        ..Default::default()
    });
    result
}

#[tokio::test]
async fn session_start_consumes_then_applies_outcome_in_order() {
    let hooks = Arc::new(Hooks::default());
    hooks.results.lock().unwrap().insert(
        HookEventKind::SessionStart,
        start_result(vec!["/tmp/a", "/tmp/b"], Some("hello")),
    );
    let (_dir, fixture) = fixture_with_hooks(Some(hooks.clone())).await;
    assert_eq!(
        hooks.seen.lock().unwrap().as_slice(),
        &[HookEventKind::Setup, HookEventKind::SessionStart]
    );
    assert_eq!(
        fixture.host.watch_paths(),
        vec![
            std::path::PathBuf::from("/tmp/a"),
            std::path::PathBuf::from("/tmp/b")
        ]
    );
    assert!(
        matches!(&fixture.host.live.lock().unwrap().journal.queue[0], In::UserText { text, .. } if text == "hello")
    );
    fixture.close().await.unwrap();
}

fn end_result(messages: Vec<HookMessage>) -> HookDispatchResult {
    let mut result = HookDispatchResult::empty(HookEventKind::SessionEnd);
    result.common.messages = messages;
    result
}

fn message(kind: HookMessageKind, content: &str) -> HookMessage {
    HookMessage {
        hook_id: "h1".into(),
        kind,
        content: content.into(),
    }
}

#[tokio::test]
async fn session_end_aggregates_non_blocking_errors() {
    let hooks = Arc::new(Hooks::default());
    hooks.results.lock().unwrap().insert(
        HookEventKind::SessionEnd,
        end_result(vec![
            message(HookMessageKind::Success, "all good"),
            message(HookMessageKind::NonBlockingError, "err-1"),
            message(HookMessageKind::NonBlockingError, "err-2"),
        ]),
    );
    let (_dir, fixture) = fixture_with_hooks(Some(hooks.clone())).await;
    fixture.close().await.unwrap();
    assert_eq!(
        fixture
            .host
            .live
            .lock()
            .unwrap()
            .journal
            .last_error
            .clone()
            .as_deref(),
        Some("err-1\nerr-2")
    );
    assert_eq!(hooks.shutdown.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn session_end_without_errors_reports_none() {
    let hooks = Arc::new(Hooks::default());
    let (_dir, fixture) = fixture_with_hooks(Some(hooks.clone())).await;
    fixture.close().await.unwrap();
    assert!(fixture
        .host
        .live
        .lock()
        .unwrap()
        .journal
        .last_error
        .clone()
        .is_none());
    assert_eq!(hooks.shutdown.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn session_end_captures_dispatch_failure_and_releases_resources() {
    let hooks = Arc::new(Hooks::default());
    let (_dir, fixture) = fixture_with_hooks(Some(hooks.clone())).await;
    *hooks.fail.lock().unwrap() = Some(HookEventKind::SessionEnd);
    fixture.close().await.unwrap();
    assert!(fixture
        .host
        .live
        .lock()
        .unwrap()
        .journal
        .last_error
        .clone()
        .unwrap()
        .contains("boom"));
    assert_eq!(fixture.host.status(), SessionStatus::Closed);
    assert_eq!(hooks.shutdown.load(Ordering::SeqCst), 1);
}
#[tokio::test]
async fn turn_completed_is_gated_on_new_history_rows() {
    let hooks = Arc::new(Hooks::default());
    let (_dir, fixture) = fixture_with_hooks(Some(hooks.clone())).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    fixture
        .host
        .notify_turn_completed(&TurnId::new(), 0, &tx)
        .await;
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::TurnCompleted));
    assert!(rx.try_recv().is_err());
    fixture.close().await.unwrap();
}
async fn append_history(host: &SessionHost) {
    let mut row = StoredMessage::new(ChatMessage::assistant("done"));
    row.seq = 9;
    host.agent()
        .ctx()
        .context_manager()
        .unwrap()
        .append(vec![row])
        .await
        .unwrap();
}
#[tokio::test]
async fn turn_completed_forwards_all_visible_output_as_notices() {
    let hooks = Arc::new(Hooks::default());
    let mut result = HookDispatchResult::empty(HookEventKind::TurnCompleted);
    result.common.system_messages = vec!["sys".into()];
    result.common.blocking_errors = vec![HookBlockingError {
        hook_id: "h1".into(),
        message: "blocked".into(),
    }];
    result.common.messages = vec![message(HookMessageKind::Success, "visible")];
    hooks.results.lock().unwrap().insert(result.event, result);
    let (_dir, fixture) = fixture_with_hooks(Some(hooks)).await;
    append_history(&fixture.host).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    fixture
        .host
        .notify_turn_completed(&TurnId::new(), 3, &tx)
        .await;
    let mut notices = vec![];
    while let Ok(Out::Notice {
        level: Level::Warning,
        message,
    }) = rx.try_recv()
    {
        notices.push(message);
    }
    assert_eq!(notices, vec!["sys", "visible", "blocked"]);
    fixture.close().await.unwrap();
}
#[tokio::test]
async fn turn_completed_downgrades_dispatch_failure_to_notice() {
    let hooks = Arc::new(Hooks::default());
    *hooks.fail.lock().unwrap() = Some(HookEventKind::TurnCompleted);
    let (_dir, fixture) = fixture_with_hooks(Some(hooks)).await;
    append_history(&fixture.host).await;
    let (tx, mut rx) = mpsc::unbounded_channel();
    fixture
        .host
        .notify_turn_completed(&TurnId::new(), 3, &tx)
        .await;
    assert!(
        matches!(rx.try_recv().unwrap(), Out::Notice { message, .. } if message.contains("TurnCompleted hook failed"))
    );
    assert!(rx.try_recv().is_err());
    fixture.close().await.unwrap();
}
