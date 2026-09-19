#[path = "../../yourai-loop/tests/support/mod.rs"]
mod support;
use std::sync::{
    atomic::{AtomicUsize, Ordering},
    Arc, Mutex,
};
use support::{answer, Model};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::*;

#[derive(Default)]
struct Memory {
    initial: AtomicUsize,
    queries: Mutex<Vec<String>>,
    fail: bool,
}
impl MemoryProvider for Memory {
    fn system_prompt_block<'a>(
        &'a self,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Option<String>, YourAiError>> {
        self.initial.fetch_add(1, Ordering::SeqCst);
        Box::pin(async move {
            if self.fail {
                return Err(ErrorKind::Config("offline".into()).into());
            }
            Ok(Some("EXTERNAL-BASE".into()))
        })
    }
    fn recall<'a>(
        &'a self,
        r: RecallRequest<'a>,
        _: &'a CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<RecalledMemory>, YourAiError>> {
        self.queries.lock().unwrap().push(r.query.into());
        Box::pin(async move {
            if self.fail {
                return Err(ErrorKind::Config("offline".into()).into());
            }
            let item = RecalledMemory {
                provider: "test".into(),
                id: r.query.into(),
                content: format!("RECALL:{}", r.query),
                updated_at: Some(1),
            };
            Ok(vec![item.clone(), item])
        })
    }
}
async fn turn(h: &Harness, text: &str) {
    h.host.submit(In::user_text(text)).unwrap();
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
}
#[tokio::test]
async fn frozen_prompt_and_recall_replay_survive_file_changes_restart_and_fork() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    for (name, value) in [
        ("SOUL.md", "SOUL-FIRST"),
        ("Memory.md", "FACTS"),
        ("profile.md", "PROFILE"),
        ("AGENTS.md", "PROJECT"),
    ] {
        std::fs::write(d.path().join(name), value).unwrap();
    }
    let memory = Arc::new(Memory::default());
    let model = Arc::new(Model::new(vec![answer("one"), answer("two")]));
    let mut config = HarnessConfig::new(root.clone(), d.path().into());
    config.instructions = vec!["AGENTS.md".into(), "Memory.md".into()];
    config.memory_provider = Some(memory.clone());
    config.memory_search_limit = 3;
    let h = Harness::open(config, model.clone()).await.unwrap();
    turn(&h, "A").await;
    let first = model.requests.lock().unwrap()[0].request.clone();
    let system = first.system.clone().unwrap();
    assert!(system.starts_with("[SOUL.md]\nSOUL-FIRST"));
    let positions = [
        "SOUL-FIRST",
        "PROJECT",
        "Initial environment",
        "FACTS",
        "PROFILE",
        "EXTERNAL-BASE",
    ]
    .map(|v| system.find(v).unwrap());
    assert!(positions.windows(2).all(|p| p[0] < p[1]));
    assert_eq!(system.matches("FACTS").count(), 1);
    for name in ["SOUL.md", "Memory.md", "profile.md", "AGENTS.md"] {
        std::fs::remove_file(d.path().join(name)).unwrap();
    }
    turn(&h, "B").await;
    let second = model.requests.lock().unwrap()[1].request.clone();
    assert_eq!(second.system.as_ref(), Some(&system));
    assert_eq!(
        serde_json::to_value(&first.messages[0]).unwrap(),
        serde_json::to_value(&second.messages[0]).unwrap()
    );
    assert_eq!(
        second
            .messages
            .iter()
            .filter(|m| m.role == ChatRole::User)
            .count(),
        2
    );
    let store = SessionCatalog::new(&root).unwrap();
    let id = h.host.context().id;
    let rows = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(rows[0].message.content.first_text(), Some("A"));
    assert_eq!(rows[0].recall.len(), 1);
    assert!(serde_json::to_string(&rows[0].api_content)
        .unwrap()
        .contains("RECALL:A"));
    assert_eq!(memory.initial.load(Ordering::SeqCst), 1);
    assert_eq!(*memory.queries.lock().unwrap(), vec!["A", "B"]);
    let fork = store.fork_session(&id).await.unwrap();
    assert_eq!(
        store
            .load_session(&fork)
            .await
            .unwrap()
            .system_prompt
            .as_ref(),
        Some(&system)
    );
    assert_eq!(
        serde_json::to_value(
            store
                .read_messages(&fork, MessageQuery::default())
                .await
                .unwrap()
                .messages[0]
                .model_message()
        )
        .unwrap(),
        serde_json::to_value(rows[0].model_message()).unwrap()
    );
    // Generic metadata updates cannot alter a frozen system.
    let mut meta = store.load_session(&id).await.unwrap();
    meta.system_prompt = Some("overwrite".into());
    store.save_session(&meta).await.unwrap();
    assert_eq!(
        store
            .initialize_system(&id, "also overwrite")
            .await
            .unwrap(),
        system
    );
    h.close().await.unwrap();
    drop(h);
    let offline = Arc::new(Memory {
        fail: true,
        ..Default::default()
    });
    let resumed_model = Arc::new(Model::new(vec![answer("three")]));
    let mut config = HarnessConfig::new(root, d.path().into());
    config.resume = Some(id);
    config.system_prompt = Some("changed configuration".into());
    config.prompt.soul_file = Some(d.path().join("missing.md"));
    config.instructions = vec!["AGENTS.md".into()];
    config.memory_provider = Some(offline.clone());
    config.memory_search_limit = 3;
    let h = Harness::open(config, resumed_model.clone()).await.unwrap();
    turn(&h, "C").await;
    assert_eq!(offline.initial.load(Ordering::SeqCst), 0);
    let replay = resumed_model.requests.lock().unwrap()[0].request.clone();
    assert_eq!(replay.system.as_ref(), Some(&system));
    assert_eq!(
        serde_json::to_value(&replay.messages[0]).unwrap(),
        serde_json::to_value(&first.messages[0]).unwrap()
    );
    assert_eq!(
        replay.messages.last().unwrap().content.first_text(),
        Some("C")
    );
    h.close().await.unwrap();
}

#[tokio::test]
async fn no_provider_works_and_explicit_missing_or_oversize_files_fail_before_session_creation() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    let mut bad = HarnessConfig::new(root.clone(), d.path().into());
    bad.prompt.profile_file = Some("missing.md".into());
    assert!(Harness::open(bad, Arc::new(Model::new(vec![])))
        .await
        .is_err());
    assert!(SessionCatalog::new(&root)
        .unwrap()
        .list_sessions()
        .await
        .unwrap()
        .is_empty());
    std::fs::write(d.path().join("Memory.md"), "too long").unwrap();
    let mut bad = HarnessConfig::new(root.clone(), d.path().into());
    bad.prompt.memory_file_max_chars = 1;
    assert!(Harness::open(bad, Arc::new(Model::new(vec![])))
        .await
        .is_err());
    let model = Arc::new(Model::new(vec![answer("ok")]));
    let h = Harness::open(HarnessConfig::new(root, d.path().into()), model.clone())
        .await
        .unwrap();
    turn(&h, "hello").await;
    assert_eq!(model.requests.lock().unwrap()[0].request.messages.len(), 1);
    h.close().await.unwrap();
}

struct SummaryCapture(Mutex<Vec<ChatRequest>>);
impl ModelProvider for SummaryCapture {
    fn model_iden(&self) -> &str {
        "summary"
    }
    fn stream_events<'a>(
        &'a self,
        _: ModelRequest,
    ) -> BoxFuture<'a, Result<yourai_core::model::ModelEventStream, YourAiError>> {
        Box::pin(async { Err(ErrorKind::Config("unused".into()).into()) })
    }
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.0.lock().unwrap().push(r.request);
        Box::pin(async {
            let id = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "summary");
            Ok(ChatResponse {
                content: MessageContent::from_text("Retained facts"),
                reasoning_content: None,
                model_iden: id.clone(),
                provider_model_iden: id,
                stop_reason: None,
                usage: GenaiUsage::default(),
                captured_raw_body: None,
                response_id: None,
            })
        })
    }
}
#[tokio::test]
async fn compaction_summarizes_sent_memory_but_never_session_system() {
    let d = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&d.path().join("db")).unwrap());
    let id = store
        .create_session("FROZEN-DO-NOT-SUMMARIZE")
        .await
        .unwrap()
        .id;
    let c = MemoryContext::new(
        id.clone(),
        yourai_harness::context::ContextServices {
            store: Some(store.clone()),
            policy: ContextPolicy {
                context_window: Some(32000),
                keep_recent_tokens: 0,
                summary_min_savings: 1,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    c.restore().await.unwrap();
    let mut old = StoredMessage::new(ChatMessage::user("old question"));
    old.attach_recall(vec![RecalledMemory {
        provider: "test".into(),
        id: "old".into(),
        content: "IMPORTANT-RECALL ".repeat(600),
        updated_at: None,
    }]);
    c.append(vec![
        old.clone(),
        StoredMessage::new(ChatMessage::assistant("done")),
        StoredMessage::new(ChatMessage::user("latest")),
    ])
    .await
    .unwrap();
    let model = Arc::new(SummaryCapture(Mutex::new(vec![])));
    let execution = ContextExecution {
        model: model.clone(),
        hooks: None,
        usage: None,
        hook_base: BaseInput::new(&id.0, ""),
    };
    let before = c.build_request(&[], &execution).unwrap();
    assert!(before.estimated_tokens > 2000);
    let result = c
        .compact(
            CompactionRequest::new(CompactionTrigger::Manual),
            &execution,
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(result.action, CompactAction::Summarized);
    let requests = serde_json::to_string(&*model.0.lock().unwrap()).unwrap();
    assert!(requests.contains("IMPORTANT-RECALL"));
    assert!(!requests.contains("FROZEN-DO-NOT-SUMMARIZE"));
    assert_eq!(
        c.build_request(&[], &execution)
            .unwrap()
            .request
            .system
            .as_deref(),
        Some("FROZEN-DO-NOT-SUMMARIZE")
    );
    c.restore().await.unwrap();
    let archived = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(archived[0].status, MessageStatus::Compacted);
    assert_eq!(
        serde_json::to_value(&archived[0].api_content).unwrap(),
        serde_json::to_value(&old.api_content).unwrap()
    );
}

#[tokio::test]
async fn multimodal_content_and_recall_are_committed_and_replayed_together() {
    let d = TempDir::new().unwrap();
    let store = SqliteStore::open(&d.path().join("db")).unwrap();
    let id = store.create_session("fixed").await.unwrap().id;
    // Multiple original content blocks must retain their order and identity.
    let original = MessageContent::from_parts(vec![
        ContentPart::from_text("question"),
        ContentPart::Binary(Binary::from_url(
            "image/png",
            "https://example.invalid/picture.png",
            None,
        )),
    ]);
    let mut row = StoredMessage::new(ChatMessage::user(original.clone()));
    row.attach_recall(vec![RecalledMemory {
        provider: "p".into(),
        id: "id".into(),
        content: "context".into(),
        updated_at: None,
    }]);
    let saved = store.append_messages(&id, vec![row.clone()]).await.unwrap();
    store.append_messages(&id, vec![row.clone()]).await.unwrap();
    let loaded = store
        .read_messages(&id, MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(loaded.len(), 1);
    assert_eq!(
        serde_json::to_value(&loaded[0].message.content).unwrap(),
        serde_json::to_value(original).unwrap()
    );
    assert_eq!(
        serde_json::to_value(loaded[0].model_message()).unwrap(),
        serde_json::to_value(saved[0].model_message()).unwrap()
    );
    row.attach_recall(vec![RecalledMemory {
        provider: "p".into(),
        id: "id".into(),
        content: "changed".into(),
        updated_at: None,
    }]);
    assert!(store.append_messages(&id, vec![row]).await.is_err());
}

#[tokio::test]
async fn request_retry_reuses_exact_recall_without_querying_again() {
    let d = TempDir::new().unwrap();
    let failed: yourai_core::model::ModelEventStream =
        Box::pin(futures_util::stream::iter(vec![Err(ErrorKind::Config(
            "temporary".into(),
        )
        .into())]));
    let mut model = Model::new(vec![failed, answer("ok")]);
    model.recovery = yourai_core::model::ModelRecovery::Retry;
    let model = Arc::new(model);
    let memory = Arc::new(Memory::default());
    let mut cfg = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    cfg.memory_provider = Some(memory.clone());
    cfg.memory_search_limit = 3;
    let h = Harness::open(cfg, model.clone()).await.unwrap();
    turn(&h, "query").await;
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::to_value(&requests[0].request).unwrap(),
            serde_json::to_value(&requests[1].request).unwrap()
        );
    }
    assert_eq!(*memory.queries.lock().unwrap(), vec!["query"]);
    h.close().await.unwrap();
}
