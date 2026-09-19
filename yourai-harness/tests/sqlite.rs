use std::sync::Arc;
use tempfile::TempDir;
use yourai_core::prelude::*;
use yourai_harness::{storage::LocalUsage, SqliteStore};
fn open(dir: &TempDir) -> SqliteStore {
    SqliteStore::open(&SqliteStore::path(dir.path())).unwrap()
}
#[tokio::test]
async fn append_is_atomic_idempotent_and_cursor_survives_restart() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let id = store.create_session("").await.unwrap().id;
    let a = StoredMessage::new(ChatMessage::user("first"));
    let b = StoredMessage::new(ChatMessage::assistant("second"));
    let rows = store
        .append_messages(&id, vec![a.clone(), b.clone()])
        .await
        .unwrap();
    assert_eq!(rows.iter().map(|m| m.seq).collect::<Vec<_>>(), vec![1, 2]);
    assert_eq!(
        store.append_messages(&id, vec![a.clone()]).await.unwrap()[0].seq,
        1
    );
    let mut conflict = a;
    conflict.message = ChatMessage::user("changed");
    assert!(store
        .append_messages(
            &id,
            vec![
                StoredMessage::new(ChatMessage::user("must roll back")),
                conflict
            ]
        )
        .await
        .is_err());
    drop(store);
    let store = open(&dir);
    let page = store
        .read_messages(
            &id,
            MessageQuery {
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.messages.len(), 1);
    assert_eq!(page.next, Some(1));
    let page = store
        .read_messages(
            &id,
            MessageQuery {
                after: page.next.unwrap(),
                limit: 1,
                ..Default::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(page.messages[0].id, b.id);
    assert_eq!(page.next, None);
}
fn summary(text: &str) -> StoredMessage {
    let mut row = StoredMessage::new(ChatMessage::user(text));
    row.summary = true;
    row
}
#[tokio::test]
async fn compaction_preserves_unselected_messages_and_rolls_back_stale_candidates() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let id = store.create_session("").await.unwrap().id;
    let rows = store
        .append_messages(
            &id,
            vec![
                StoredMessage::new(ChatMessage::user("protected head")),
                StoredMessage::new(ChatMessage::user("old turn")),
                StoredMessage::new(ChatMessage::user("recent turn")),
            ],
        )
        .await
        .unwrap();
    let first = summary("first summary");
    let change = CompactionChange {
        sources: vec![rows[1].id.clone()],
        summary: first.clone(),
    };
    store
        .save_context(&id, change.clone().into())
        .await
        .unwrap();
    store.save_context(&id, change.into()).await.unwrap();
    assert!(store
        .save_context(
            &id,
            CompactionChange {
                sources: vec![rows[0].id.clone(), rows[1].id.clone()],
                summary: summary("stale")
            }
            .into()
        )
        .await
        .is_err());
    let active = store
        .read_messages(
            &id,
            MessageQuery {
                active_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .messages;
    assert_eq!(active.len(), 3);
    assert!(active.iter().any(|m| m.id == rows[0].id));
    // Omitting the previous active summary violates the unique index; source updates roll back.
    assert!(store
        .save_context(
            &id,
            CompactionChange {
                sources: vec![rows[2].id.clone()],
                summary: summary("invalid")
            }
            .into()
        )
        .await
        .is_err());
    store
        .save_context(
            &id,
            CompactionChange {
                sources: vec![first.id, rows[2].id.clone()],
                summary: summary("replacement"),
            }
            .into(),
        )
        .await
        .unwrap();
    let all = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(all.len(), 5);
    assert_eq!(
        all.iter()
            .filter(|m| m.status == MessageStatus::Active)
            .count(),
        2
    );
}
#[tokio::test]
async fn usage_is_deduplicated_and_unknown_counters_stay_null() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let id = store.create_session("").await.unwrap().id;
    let usage = LocalUsage(store);
    let event = UsageEvent::new(
        Some("model".into()),
        "compact",
        GenaiUsage {
            prompt_tokens: Some(7),
            ..Default::default()
        },
    );
    usage.record_event(&id, &event).await.unwrap();
    usage.record_event(&id, &event).await.unwrap();
    let mut conflict = event;
    conflict.source = "main".into();
    assert!(usage.record_event(&id, &conflict).await.is_err());
    let stats = usage.total().await.unwrap();
    assert_eq!(stats.request_count, 1);
    assert_eq!(stats.total_input_tokens, 7);
    let db = rusqlite::Connection::open(SqliteStore::path(dir.path())).unwrap();
    let counters: (Option<i64>, Option<i64>) = db
        .query_row(
            "SELECT output_tokens,total_tokens FROM usage_events",
            [],
            |r| Ok((r.get(0)?, r.get(1)?)),
        )
        .unwrap();
    assert_eq!(counters, (None, None));
}
#[tokio::test]
async fn fork_copies_history_with_new_identities_and_delete_cascades() {
    let dir = TempDir::new().unwrap();
    let store = open(&dir);
    let id = store.create_session("").await.unwrap().id;
    let original = store
        .append_messages(&id, vec![StoredMessage::new(ChatMessage::user("hello"))])
        .await
        .unwrap();
    let child = store.fork_session(&id).await.unwrap();
    assert_eq!(
        store.load_session(&child).await.unwrap().parent_session_id,
        Some(id.clone())
    );
    let copied = store
        .read_messages(&child, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_ne!(original[0].id, copied[0].id);
    let usage = LocalUsage(store.clone());
    usage
        .record_event(&id, &UsageEvent::new(None, "main", GenaiUsage::default()))
        .await
        .unwrap();
    store.delete_session(&id).await.unwrap();
    assert_eq!(usage.total().await.unwrap().request_count, 0);
    assert_eq!(
        store
            .read_messages(&child, MessageQuery::default())
            .await
            .unwrap()
            .messages
            .len(),
        1
    );
}
#[tokio::test]
async fn schema_separates_request_diagnostics_from_history() {
    let dir = TempDir::new().unwrap();
    let _store = open(&dir);
    let db = rusqlite::Connection::open(SqliteStore::path(dir.path())).unwrap();
    let mut q = db
        .prepare("SELECT name FROM sqlite_master WHERE type='table' ORDER BY name")
        .unwrap();
    let tables = q
        .query_map([], |r| r.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        tables,
        vec!["messages", "model_requests", "sessions", "usage_events"]
    );
    let ddl: String = db
        .query_row(
            "SELECT sql FROM sqlite_master WHERE name='messages'",
            [],
            |r| r.get(0),
        )
        .unwrap();
    assert!(!ddl.contains("source_message_ids"));
    assert!(!ddl.contains("tenant"));
}
#[tokio::test]
async fn concurrent_writers_allocate_unique_sequence_numbers() {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(open(&dir));
    let id = store.create_session("").await.unwrap().id;
    let mut tasks = vec![];
    for i in 0..20 {
        let store = store.clone();
        let id = id.clone();
        tasks.push(tokio::spawn(async move {
            store
                .append_messages(
                    &id,
                    vec![StoredMessage::new(ChatMessage::user(i.to_string()))],
                )
                .await
                .unwrap();
        }));
    }
    for task in tasks {
        task.await.unwrap();
    }
    let rows = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(
        rows.iter().map(|m| m.seq).collect::<Vec<_>>(),
        (1..=20).collect::<Vec<_>>()
    );
}

#[tokio::test]
async fn legacy_import_preserves_sources_and_is_transactional() {
    let dir = TempDir::new().unwrap();
    let session = dir.path().join("legacy");
    std::fs::create_dir(&session).unwrap();
    std::fs::write(
        session.join("meta.json"),
        r#"{"id":"legacy","title":"old","created":1,"updated":2,"model":"old-model"}"#,
    )
    .unwrap();
    let history = serde_json::json!({"revision":4,"messages":[ChatMessage::system("reload me"),ChatMessage::user("old"),ChatMessage::assistant("answer"),ChatMessage::user("tail")],"summary":"summary","boundary":3,"usage":[{"input_tokens":3,"output_tokens":2,"total_tokens":5}]});
    std::fs::write(
        session.join("history.json"),
        serde_json::to_vec(&history).unwrap(),
    )
    .unwrap();
    assert!(yourai_harness::SessionCatalog::new(dir.path()).is_err());
    assert_eq!(
        yourai_harness::storage::sqlite::import_json_sessions(dir.path()).unwrap(),
        1
    );
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(
            &std::fs::read(session.join("history.json")).unwrap()
        )
        .unwrap(),
        history
    );
    let catalog = yourai_harness::SessionCatalog::new(dir.path()).unwrap();
    let id = SessionId("legacy".into());
    assert_eq!(catalog.load_session(&id).await.unwrap().created_at, 1000);
    let active = catalog
        .read_messages(
            &id,
            MessageQuery {
                active_only: true,
                ..Default::default()
            },
        )
        .await
        .unwrap()
        .messages;
    assert_eq!(active.len(), 2);
    assert_eq!(
        LocalUsage(open(&dir)).total().await.unwrap().total_tokens,
        5
    );
    assert!(yourai_harness::storage::sqlite::import_json_sessions(dir.path()).is_err());
    assert_eq!(
        catalog
            .read_messages(&id, MessageQuery::default())
            .await
            .unwrap()
            .messages
            .len(),
        4
    );
}

#[tokio::test]
async fn version_one_migrates_pruning_column_without_changing_original_messages() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("migrate.sqlite");
    let store = SqliteStore::open(&path).unwrap();
    let id = store.create_session("").await.unwrap().id;
    store
        .append_messages(
            &id,
            vec![StoredMessage::new(ChatMessage::user("preserved"))],
        )
        .await
        .unwrap();
    drop(store);
    let db = rusqlite::Connection::open(&path).unwrap();
    db.execute_batch(
        "ALTER TABLE messages DROP COLUMN tool_output_pruned_at; ALTER TABLE sessions DROP COLUMN system_prompt; PRAGMA user_version=1;",
    )
    .unwrap();
    drop(db);
    let store = SqliteStore::open(&path).unwrap();
    let rows = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(rows[0].message.content.first_text(), Some("preserved"));
    assert!(rows[0].tool_output_pruned_at.is_none());
    assert!(store
        .load_session(&id)
        .await
        .unwrap()
        .system_prompt
        .is_none());
    assert_eq!(
        store
            .initialize_system(&id, "migration snapshot")
            .await
            .unwrap(),
        "migration snapshot"
    );
    assert_eq!(
        store
            .initialize_system(&id, "must not overwrite")
            .await
            .unwrap(),
        "migration snapshot"
    );

    let db = rusqlite::Connection::open(&path).unwrap();
    assert_eq!(
        db.query_row("PRAGMA user_version", [], |r| r.get::<_, i64>(0))
            .unwrap(),
        4
    );
}
