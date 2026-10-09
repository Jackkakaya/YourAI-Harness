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
fn running(host: &SessionHost) -> mpsc::UnboundedReceiver<In> {
    let (tx, rx) = mpsc::unbounded_channel();
    let mut live = host.live.lock().unwrap();
    live.status = SessionStatus::Running {
        turn_id: TurnId::new(),
    };
    live.inbox = Some(tx);
    rx
}
fn idle(host: &SessionHost) {
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
    let reopened = SessionHost::open(
        host.dir.clone(),
        host.context(),
        host.agent.clone(),
        HostConfig::default(),
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
    let durable: Journal = read_json(&host.dir.join("host.json")).unwrap();
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
    let path = host.dir.join("host.json");
    let backup = path.with_extension("backup");
    std::fs::rename(&path, &backup).unwrap();
    std::fs::create_dir(&path).unwrap();
    assert!(host
        .steer_pending(pending[0].id().unwrap().into())
        .await
        .is_err());
    assert!(rx.try_recv().is_err());
    assert_eq!(wire(&host.pending_inputs()), wire(&pending));
    std::fs::remove_dir(&path).unwrap();
    std::fs::rename(backup, path).unwrap();
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
    let durable: Journal = read_json(&host.dir.join("host.json")).unwrap();
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
    let path = host.dir.join("host.json");
    let mut journal: Journal = read_json(&path).unwrap();
    journal.queue.push_back(attached("legacy"));
    atomic_write(&path, &journal).unwrap();
    let reopened = SessionHost::open(
        host.dir.clone(),
        host.context(),
        host.agent.clone(),
        HostConfig::default(),
        "resume",
    )
    .await
    .unwrap();
    let pending = reopened.pending_inputs();
    assert!(pending[0].id().is_some_and(|id| !id.is_empty()));
    let durable: Journal = read_json(&path).unwrap();
    assert_eq!(
        wire(&durable.queue.into_iter().collect::<Vec<_>>()),
        wire(&pending)
    );
    assert_eq!(wire(&reopened.close(None).await.unwrap()), wire(&pending));
}
