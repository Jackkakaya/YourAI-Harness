#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::{
    path::Path,
    sync::{Arc, Barrier, Mutex},
};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{
    hooks::NativeHandler,
    memory::LocalMemory,
    skills::LocalSkills,
    tools::{Read, ToolOutputStore},
    Harness, HarnessConfig, SqliteStore,
};

#[tokio::test]
async fn state_revisions_prevent_lost_updates_across_connections() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("shared.sqlite3");
    let first = SqliteStore::open(&path).unwrap();
    let id = first.create_session(SessionId::new(), "").await.unwrap().id;
    first
        .save_state(&id, "runtime", None, json!({"queue":[]}))
        .unwrap();
    let barrier = Arc::new(Barrier::new(2));
    let workers: Vec<_> = (0..2)
        .map(|n| {
            let db = SqliteStore::open(&path).unwrap();
            let id = id.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                let old = db.read_state(&id, "runtime").unwrap().unwrap();
                barrier.wait();
                db.save_state(&id, "runtime", Some(old.revision), json!({"queue":[n]}))
            })
        })
        .collect();
    assert_eq!(
        workers
            .into_iter()
            .map(|t| t.join().unwrap().is_ok())
            .filter(|ok| *ok)
            .count(),
        1
    );
    assert_eq!(
        first.read_state(&id, "runtime").unwrap().unwrap().revision,
        2
    );
}

#[tokio::test]
async fn expired_owner_cannot_write_or_release_replacement_owner() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("shared.sqlite3");
    let db = SqliteStore::open(&path).unwrap();
    let id = db.create_session(SessionId::new(), "").await.unwrap().id;
    let old = db.claim_session(&id, "old-worker", 60_000).unwrap();
    assert!(SqliteStore::open(&path)
        .unwrap()
        .claim_session(&id, "other-worker", 60_000)
        .is_err());
    assert!(db.delete_session(&id).await.is_err());
    old.save_state(&id, "runtime", None, json!({"queue":["kept"]}))
        .unwrap();
    rusqlite::Connection::open(&path)
        .unwrap()
        .execute("UPDATE session_owners SET expires_at=0", [])
        .unwrap();
    let new = SqliteStore::open(&path)
        .unwrap()
        .claim_session(&id, "new-worker", 60_000)
        .unwrap();
    assert!(old
        .save_state(&id, "runtime", Some(1), json!({"queue":[]}))
        .is_err());
    assert!(old
        .append_messages(&id, vec![StoredMessage::new(ChatMessage::user("stale"))])
        .await
        .is_err());
    old.release_session().unwrap();
    assert!(new.renew_session(60_000).is_ok());
    assert_eq!(
        new.read_state(&id, "runtime").unwrap().unwrap().value["queue"],
        json!(["kept"])
    );
    new.release_session().unwrap();
    db.delete_session(&id).await.unwrap();
}

#[tokio::test]
async fn outputs_are_session_scoped_idempotent_and_readable_without_files() {
    let db = Arc::new(SqliteStore::open(Path::new(":memory:")).unwrap());
    let id = db.create_session(SessionId::new(), "").await.unwrap().id;
    let other = db.create_session(SessionId::new(), "").await.unwrap().id;
    let store = ToolOutputStore::new(db.clone(), id);
    let reference = store.create();
    store.append(&reference, 0, b"first-").await.unwrap();
    store.append(&reference, 0, b"first-").await.unwrap();
    assert!(store.append(&reference, 0, b"conflict").await.is_err());
    store.append(&reference, 6, b"second").await.unwrap();
    assert_eq!(store.read(&reference, 4, 5).await.unwrap(), b"t-sec");
    assert!(ToolOutputStore::new(db, other)
        .read(&reference, 0, 20)
        .await
        .is_err());
    assert!(store.read(&reference, 0, 1024 * 1024 + 1).await.is_err());
    let cancel = CancellationToken::new();
    let result = Read::with_output(Path::new("/").to_owned(), Some(store))
        .run(
            ToolContext {
                providers: None,
                cwd: None,
                call_id: "read-output".into(),
                emit: &DiscardSink,
                cancel: &cancel,
                security: None,
                sandbox: None,
                interaction: None,
            },
            json!({"path":reference,"byte_offset":6,"byte_limit":6}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"], "second");
}

#[tokio::test]
async fn output_cleanup_never_leaves_partial_live_outputs() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("outputs.sqlite3");
    let db = Arc::new(SqliteStore::open(&path).unwrap());
    let id = db.create_session(SessionId::new(), "").await.unwrap().id;
    let output = ToolOutputStore::new(db.clone(), id.clone());
    let reference = output.create();
    output.append(&reference, 0, b"old").await.unwrap();
    output.append(&reference, 3, b"fresh").await.unwrap();
    let raw = rusqlite::Connection::open(&path).unwrap();
    raw.execute("UPDATE tool_outputs SET created_at=0 WHERE offset=0", [])
        .unwrap();
    db.cleanup_outputs(&id, 1).await.unwrap();
    assert_eq!(output.read(&reference, 0, 20).await.unwrap(), b"oldfresh");
    raw.execute("UPDATE tool_outputs SET created_at=0", [])
        .unwrap();
    db.cleanup_outputs(&id, 1).await.unwrap();
    assert!(output.read(&reference, 0, 20).await.is_err());
}

#[tokio::test]
async fn memory_and_skills_survive_reopening_and_observe_other_connections() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("extensions.sqlite3");
    let first = Arc::new(SqliteStore::open(&path).unwrap());
    let id = first.create_session(SessionId::new(), "").await.unwrap().id;
    let second = Arc::new(SqliteStore::open(&path).unwrap());
    let memory = LocalMemory::new(first.clone(), id.clone());
    let peer = LocalMemory::new(second.clone(), id.clone());
    memory
        .store("language", "Chinese", Some("preference"))
        .await
        .unwrap();
    peer.store("project", "cloud agent", None).await.unwrap();
    assert_eq!(memory.list(None).await.unwrap().len(), 2);
    memory.delete("language").await.unwrap();
    assert!(peer.retrieve("language").await.unwrap().is_none());
    let skills = LocalSkills::new(first, id.clone());
    skills
        .register(SkillContent {
            info: SkillInfo {
                id: "test".into(),
                name: "test".into(),
                description: "check".into(),
                category: None,
            },
            instructions: "verify changes".into(),
            tools: vec![],
        })
        .await
        .unwrap();
    let skills = LocalSkills::new(second, id);
    assert_eq!(
        skills.load("test").await.unwrap().instructions,
        "verify changes"
    );
    assert_eq!(
        std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|p| p.path().extension().is_some_and(|e| e == "json"))
            .count(),
        0
    );
}

#[tokio::test]
async fn runtime_state_import_is_once_and_resume_no_longer_needs_json() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let config = HarnessConfig::new(root.path().to_owned(), cwd.path().to_owned());
    let first = Harness::open(config.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let id = first.host.environment().id;
    let directory = first.host.directory().to_owned();
    first.close().await.unwrap();
    let db = SqliteStore::open(&root.path().join("sessions.sqlite3")).unwrap();
    let mut state = db.read_state(&id, "runtime").unwrap().unwrap().value;
    state["queue"] = serde_json::to_value(vec![In::follow_up("legacy pending")]).unwrap();
    let file = directory.join("host.json");
    let backup = serde_json::to_vec(&state).unwrap();
    std::fs::write(&file, &backup).unwrap();
    rusqlite::Connection::open(root.path().join("sessions.sqlite3"))
        .unwrap()
        .execute("DELETE FROM session_state WHERE state_key='runtime'", [])
        .unwrap();
    let mut resume = config;
    resume.resume = Some(id.clone());
    let second = Harness::open(resume.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let input_id = second.host.pending_inputs()[0].id().unwrap().to_owned();
    assert_eq!(std::fs::read(&file).unwrap(), backup);
    second.close().await.unwrap();
    std::fs::remove_file(&file).unwrap();
    let third = Harness::open(resume, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    assert_eq!(third.host.pending_inputs()[0].id(), Some(input_id.as_str()));
    third.close().await.unwrap();
    assert!(!file.exists());
}

#[tokio::test]
async fn config_hook_receives_proposal_without_a_temporary_file() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let h = Harness::open(
        HarnessConfig::new(root.path().to_owned(), cwd.path().to_owned()),
        Arc::new(support::Model::new(vec![])),
    )
    .await
    .unwrap();
    let seen = Arc::new(Mutex::new(None));
    let copy = seen.clone();
    h.hooks
        .register(NativeHookRegistration {
            id: "inspect-config".into(),
            event: HookEventKind::ConfigChange,
            matcher: None,
            handler: Arc::new(NativeHandler::new(move |inv| {
                if let HookEvent::ConfigChange {
                    file_path, config, ..
                } = &inv.event
                {
                    assert!(file_path.is_none());
                    *copy.lock().unwrap() = config.clone();
                }
                Ok(yourai_harness::hooks::wire_output::HookJsonOutput::Sync(
                    Default::default(),
                ))
            })),
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        })
        .await
        .unwrap();
    h.host
        .workspace()
        .unwrap()
        .change_config("session", json!({"memory_search_limit":7}))
        .await
        .unwrap();
    assert_eq!(
        seen.lock().unwrap().as_ref().unwrap()["memory_search_limit"],
        7
    );
    assert!(!h.host.directory().join("config.candidate.json").exists());
    assert!(!h.host.directory().join("config.json").exists());
    h.close().await.unwrap();
}

#[tokio::test]
async fn legacy_tool_outputs_move_to_database_without_rewriting_history() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let config = HarnessConfig::new(root.path().to_owned(), cwd.path().to_owned());
    let first = Harness::open(config.clone(), Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    let id = first.host.environment().id;
    let directory = root.path().join("tool-output");
    std::fs::create_dir(&directory).unwrap();
    let path = directory.join(format!("tool_{}.txt", uuid::Uuid::new_v4()));
    let content = b"legacy full tool output";
    std::fs::write(&path, content).unwrap();
    let original =
        json!({"output_paths":[path],"stdout_path":path,"stdout":"old preview","exit_code":0})
            .to_string();
    let call = ToolCall {
        call_id: "legacy-call".into(),
        fn_name: "shell".into(),
        fn_arguments: json!({}),
        thought_signatures: None,
    };
    first
        .sessions
        .append_messages(
            &id,
            vec![
                StoredMessage::new(ChatMessage::assistant(MessageContent::from_tool_calls(
                    vec![call],
                ))),
                StoredMessage::new(ToolResponse::new("legacy-call", original.clone()).into()),
            ],
        )
        .await
        .unwrap();
    first.close().await.unwrap();
    let mut resume = config;
    resume.resume = Some(id.clone());
    let h = Harness::open(resume, Arc::new(support::Model::new(vec![])))
        .await
        .unwrap();
    assert_eq!(std::fs::read(&path).unwrap(), content);
    std::fs::remove_file(&path).unwrap();
    let snapshot = h.host.agent().ctx().snapshot().unwrap();
    let request = snapshot
        .context_manager
        .unwrap()
        .build_request(&[], snapshot.model.as_ref().unwrap().as_ref(), &[])
        .unwrap();
    let result = request
        .request
        .messages
        .iter()
        .flat_map(|m| m.content.tool_responses())
        .next()
        .unwrap();
    let projected: serde_json::Value = serde_json::from_str(&result.content).unwrap();
    let reference = projected["output_refs"][0].as_str().unwrap();
    assert!(reference.starts_with("tool-output://"));
    assert!(projected.get("output_paths").is_none());
    assert_eq!(
        ToolOutputStore::new(h.host.store(), id.clone())
            .read(reference, 0, 1024)
            .await
            .unwrap(),
        content
    );
    let stored = h
        .sessions
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap();
    assert_eq!(
        stored
            .messages
            .iter()
            .flat_map(|m| m.message.content.tool_responses())
            .next()
            .unwrap()
            .content,
        original
    );
    h.close().await.unwrap();
}
