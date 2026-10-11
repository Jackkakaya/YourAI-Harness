#![allow(dead_code)]
use std::{path::Path, sync::Arc};
use yourai_core::prelude::*;
use yourai_harness::SqliteStore;
pub async fn store() -> (Arc<SqliteStore>, SessionId) {
    let store = Arc::new(SqliteStore::open(Path::new(":memory:")).unwrap());
    let id = store.create_session(SessionId::new(), "").await.unwrap().id;
    (store, id)
}
pub fn failure(path: &Path, key: &str, enabled: bool) {
    let db = rusqlite::Connection::open(path).unwrap();
    db.execute_batch(
        "DROP TRIGGER IF EXISTS fail_state_insert; DROP TRIGGER IF EXISTS fail_state_update;",
    )
    .unwrap();
    if enabled {
        assert!(key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
        db.execute_batch(&format!("CREATE TRIGGER fail_state_insert BEFORE INSERT ON session_state WHEN NEW.state_key='{key}' BEGIN SELECT RAISE(ABORT,'injected state failure'); END; CREATE TRIGGER fail_state_update BEFORE UPDATE ON session_state WHEN NEW.state_key='{key}' BEGIN SELECT RAISE(ABORT,'injected state failure'); END;")).unwrap();
    }
}
pub fn state(path: &Path, id: &SessionId, key: &str) -> serde_json::Value {
    SqliteStore::open(path)
        .unwrap()
        .read_state(id, key)
        .unwrap()
        .unwrap()
        .value
}
