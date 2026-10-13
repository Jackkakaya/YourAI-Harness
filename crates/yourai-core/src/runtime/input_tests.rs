use super::*;
use crate::runtime::test_support::fixture;

fn attached(text: &str) -> In {
    In::UserText {
        id: None,
        text: text.into(),
        mode: InputMode::FollowUp,
        attachments: vec![UserAttachment::file("note.txt", None)],
    }
}
fn running(host: &SessionManager) -> mpsc::UnboundedReceiver<In> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut live = host.live.lock().unwrap();
    live.status = SessionStatus::Running {
        turn_id: TurnId::new(),
    };
    live.inbox = Some(tx);
    rx
}
fn idle(host: &SessionManager) {
    let mut live = host.live.lock().unwrap();
    live.status = SessionStatus::Idle;
    live.inbox = None;
}
fn wire(inputs: &[In]) -> serde_json::Value {
    serde_json::to_value(inputs).unwrap()
}

#[tokio::test]
async fn equal_messages_get_distinct_ids_and_keep_attachments_after_reopen() {
    let (_dir, harness) = fixture().await;
    let host = harness.host.clone();
    host.submit_async(attached("same")).await.unwrap();
    host.submit_async(attached("same")).await.unwrap();
    let pending = host.pending_inputs();
    assert_ne!(pending[0].id(), pending[1].id());
    let before = wire(&pending);
    assert_eq!(wire(&harness.close().await.unwrap()), before);
    let reopened = SessionManager::open(
        host.dir.clone(),
        host.environment(),
        host.agent.clone(),
        Default::default(),
        "resume",
    )
    .await
    .unwrap();
    assert_eq!(wire(&reopened.pending_inputs()), before);
    assert_eq!(wire(&reopened.close(None).await.unwrap()), before);
}

#[tokio::test]
async fn steer_transfers_only_the_selected_identity_and_rejects_stale_clicks() {
    let (_dir, harness) = fixture().await;
    let host = harness.host.clone();
    let mut rx = running(&host);
    host.submit_async(attached("same")).await.unwrap();
    host.submit_async(attached("same")).await.unwrap();
    assert!(rx.try_recv().is_err(), "ordinary input must stay queued");
    let pending = host.pending_inputs();
    let id = pending[1].id().unwrap().to_owned();
    assert!(host.steer_pending(id.clone()).await.unwrap());
    assert!(!host.steer_pending(id).await.unwrap());
    let sent = rx.try_recv().unwrap();
    let mut expected = pending[1].clone();
    if let In::UserText { mode, .. } = &mut expected {
        *mode = InputMode::Steer;
    }
    assert_eq!(wire(std::slice::from_ref(&sent)), wire(&[expected]));
    assert_eq!(wire(&host.pending_inputs()), wire(&pending[..1]));
    let durable: Journal =
        serde_json::from_value(host.read_state("runtime").unwrap().unwrap().value).unwrap();
    assert_eq!(wire(&durable.active), wire(&[sent]));
    assert!(host.submit_async(pending[0].clone()).await.is_err());
    assert!(host.submit_async(pending[1].clone()).await.is_err());
    idle(&host);
    harness.close().await.unwrap();
}

#[tokio::test]
async fn failed_steer_commit_cannot_deliver_or_remove_the_queued_message() {
    let (_dir, harness) = fixture().await;
    let host = harness.host.clone();
    let mut rx = running(&host);
    host.submit_async(attached("keep")).await.unwrap();
    let pending = host.pending_inputs();
    harness
        .store
        .fail
        .store(true, std::sync::atomic::Ordering::Relaxed);
    assert!(host
        .steer_pending(pending[0].id().unwrap().into())
        .await
        .is_err());
    assert!(rx.try_recv().is_err());
    assert_eq!(wire(&host.pending_inputs()), wire(&pending));
    harness
        .store
        .fail
        .store(false, std::sync::atomic::Ordering::Relaxed);
    idle(&host);
    harness.close().await.unwrap();
}

#[tokio::test]
async fn closed_inbox_restores_the_same_message_position_mode_and_attachments() {
    let (_dir, harness) = fixture().await;
    let host = harness.host.clone();
    drop(running(&host));
    for text in ["first", "middle", "last"] {
        host.submit_async(attached(text)).await.unwrap();
    }
    let before = host.pending_inputs();
    assert!(!host
        .steer_pending(before[1].id().unwrap().into())
        .await
        .unwrap());
    assert_eq!(wire(&host.pending_inputs()), wire(&before));
    let durable: Journal =
        serde_json::from_value(host.read_state("runtime").unwrap().unwrap().value).unwrap();
    assert!(durable.active.is_empty());
    assert_eq!(
        wire(&durable.queue.into_iter().collect::<Vec<_>>()),
        wire(&before)
    );
    idle(&host);
    harness.close().await.unwrap();
}

#[tokio::test]
async fn legacy_queue_ids_are_migrated_once_and_persisted() {
    let (_dir, harness) = fixture().await;
    let host = harness.host.clone();
    harness.close().await.unwrap();
    let state = harness
        .store
        .read_state(&host.environment().id, "runtime")
        .unwrap()
        .unwrap();
    let mut journal: Journal = serde_json::from_value(state.value).unwrap();
    journal.queue.push_back(attached("legacy"));
    harness
        .store
        .save_state(
            &host.environment().id,
            "runtime",
            Some(state.revision),
            serde_json::to_value(&journal).unwrap(),
        )
        .unwrap();
    let reopened = SessionManager::open(
        host.dir.clone(),
        host.environment(),
        host.agent.clone(),
        Default::default(),
        "resume",
    )
    .await
    .unwrap();
    let pending = reopened.pending_inputs();
    assert!(pending[0].id().is_some_and(|id| !id.is_empty()));
    let durable: Journal =
        serde_json::from_value(reopened.read_state("runtime").unwrap().unwrap().value).unwrap();
    assert_eq!(
        wire(&durable.queue.into_iter().collect::<Vec<_>>()),
        wire(&pending)
    );
    assert_eq!(wire(&reopened.close(None).await.unwrap()), wire(&pending));
}
