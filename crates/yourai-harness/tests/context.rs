#[path = "support/context.rs"]
mod context_fixture;
use context_fixture::{ContextServices, DefaultContext};
#[path = "support/execution.rs"]
mod custom;
#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_harness::{storage::LocalUsage, SqliteStore};

struct Summarizer {
    requests: Mutex<Vec<ModelRequest>>,
    empty: bool,
}
impl Summarizer {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            requests: Mutex::new(vec![]),
            empty: false,
        })
    }
}
impl ModelProvider for Summarizer {
    fn model_iden(&self) -> &str {
        "summary"
    }
    fn complete<'a>(
        &'a self,
        request: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async move {
            self.requests.lock().unwrap().push(request);
            let model = genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "summary");
            Ok(ChatResponse {
                content: MessageContent::from_text(if self.empty {
                    ""
                } else {
                    "## Goal\nDecisions and completed work.\n## Constraints and decisions\nNone\n## Completed and verified\nNone\n## Current work\nNone\n## Next steps\nNone\n## Files and commands\nNone"
                }),
                model_iden: model.clone(),
                provider_model_iden: model,
                reasoning_content: None,
                stop_reason: None,
                usage: GenaiUsage {
                    prompt_tokens: Some(100),
                    completion_tokens: Some(10),
                    total_tokens: Some(110),
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
fn policy() -> ContextPolicy {
    ContextPolicy {
        safety_margin: 100,
        advance_tokens: Some(2000),
        keep_recent_tokens: 0,
        summary_min_savings: 10,
        ..Default::default()
    }
}
async fn setup(
    window: Option<u64>,
    output: u32,
    p: ContextPolicy,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookManager>>,
) -> (TempDir, Arc<SqliteStore>, Arc<DefaultContext>) {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("db.sqlite")).unwrap());
    let id = store.create_session(SessionId::new(), "").await.unwrap().id;
    let mut services = ContextServices::new(&id);
    services.store = Some(store.clone());
    services.policy = p;
    services.hooks = hooks;
    services.usage = Some(Arc::new(LocalUsage((*store).clone())));
    let context = DefaultContext::new(
        id,
        context_fixture::configured(model, window, output),
        services,
    );
    context.restore().await.unwrap();
    (dir, store, context)
}
async fn append(c: &DefaultContext, messages: Vec<ChatMessage>) {
    c.append(messages.into_iter().map(StoredMessage::new).collect())
        .await
        .unwrap();
}
fn manual() -> CompactionRequest {
    CompactionRequest::new(CompactionTrigger::Manual)
}
fn call(id: &str) -> ToolCall {
    ToolCall {
        call_id: id.into(),
        fn_name: "test".into(),
        fn_arguments: json!({}),
        thought_signatures: None,
    }
}
fn tool(id: &str, content: String) -> ChatMessage {
    ToolResponse::new(id, content).into()
}
async fn seed_tools(c: &DefaultContext) {
    append(
        c,
        vec![
            ChatMessage::user("old task"),
            vec![call("a"), call("b")].into(),
            tool(
                "a",
                json!({"ok":true,"output":"x".repeat(6000)}).to_string(),
            ),
            tool(
                "b",
                json!({"ok":false,"error":"keep this failure"}).to_string(),
            ),
        ],
    )
    .await;
    let response = StoredMessage::new(ChatMessage::assistant("read both outputs"));

    c.append(vec![
        response,
        StoredMessage::new(ChatMessage::user("current user request")),
    ])
    .await
    .unwrap();
}

#[tokio::test]
async fn custom_loop_compaction_uses_summary_lifecycle_and_retains_current_input() {
    let hooks = Arc::new(support::Hooks::new(|_, result| {
        if let HookPointOutcome::Generic = &mut result.outcome {
            result
                .common
                .additional_contexts
                .push("compaction context".into());
        }
    }));
    let model = Summarizer::new();
    let (_dir, _store, history) = setup(Some(12_000), 500, policy(), model.clone(), None).await;
    append(
        &history,
        vec![
            ChatMessage::user("old task ".repeat(300)),
            ChatMessage::assistant("old answer ".repeat(300)),
        ],
    )
    .await;
    let agent = Agent::builder()
        .agent_loop(Arc::new(custom::CompactLoop))
        .model(context_fixture::configured(
            model.clone(),
            Some(12_000),
            500,
        ))
        .context_manager(history.clone())
        .hooks(hooks.clone())
        .build();
    let output = agent.run(In::user_text("current input")).await.unwrap();
    assert_eq!(output.text, "compacted");
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [
            HookEventKind::UserPromptSubmit,
            HookEventKind::PreCompact,
            HookEventKind::PostCompact,
            HookEventKind::Stop,
        ]
    );
    let records = history.records();
    assert!(records.iter().any(|r| r.summary));
    assert!(records.iter().any(|r| r.runtime_context
        && r.message
            .content
            .texts()
            .join("")
            .contains("compaction context")));
    assert!(records.iter().any(|r| r.message.role == ChatRole::User
        && r.message.content.texts().join("") == "current input"));
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0]
        .request
        .system
        .as_deref()
        .unwrap()
        .contains("compaction context"));
}
#[tokio::test]
async fn request_projection_preserves_output_without_a_second_truncation() {
    let (_dir, _store, c) = setup(Some(120_000), 500, policy(), Summarizer::new(), None).await;
    let original = json!({"ok":true,"output":"你好🌍".repeat(10_000)}).to_string();
    append(
        &c,
        vec![vec![call("a")].into(), tool("a", original.clone())],
    )
    .await;
    for _ in 0..2 {
        let projected = c.build_request(&[]).unwrap();
        assert_eq!(
            projected.request.messages[1].content.tool_responses()[0].content,
            original
        );
    }
    assert_eq!(
        c.records()[1].message.content.tool_responses()[0].content,
        original
    );
}

#[tokio::test]
async fn summary_keeps_original_tool_archive_and_survives_restore_and_fork() {
    let mut p = policy();
    p.advance_tokens = Some(2200);

    let model = Summarizer::new();
    let hooks = Arc::new(support::Hooks::new(|_, _| {}));
    let (_dir, store, c) = setup(
        Some(4000),
        500,
        p.clone(),
        model.clone(),
        Some(hooks.clone()),
    )
    .await;
    seed_tools(&c).await;
    let result = c
        .compact(
            CompactionRequest::new(CompactionTrigger::Threshold),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert!(result.compacted);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    assert_eq!(
        *hooks.seen.lock().unwrap(),
        [HookEventKind::PreCompact, HookEventKind::PostCompact]
    );
    assert!(c.records()[0].summary);
    let archive = store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap();
    assert!(archive
        .messages
        .iter()
        .flat_map(|m| m.message.content.tool_responses())
        .any(|r| r.content.contains(&"x".repeat(6000))));
    let expected = serde_json::to_value(c.build_request(&[]).unwrap().request).unwrap();
    c.restore().await.unwrap();
    assert_eq!(
        expected,
        serde_json::to_value(c.build_request(&[]).unwrap().request).unwrap()
    );
    let child = store.fork_session(c.session_id()).await.unwrap();
    let mut services = ContextServices::new(&child);
    services.policy = p;
    services.store = Some(store);
    let fork = DefaultContext::new(
        child,
        context_fixture::configured(model, Some(4000), 500),
        services,
    );
    fork.restore().await.unwrap();
    assert_eq!(
        expected,
        serde_json::to_value(fork.build_request(&[]).unwrap().request).unwrap()
    );
}
#[tokio::test]
async fn failed_summary_commit_keeps_history_unapplied_but_accounts_usage() {
    let mut p = policy();
    p.advance_tokens = Some(3300);

    let model = Summarizer::new();
    let (dir, store, c) = setup(Some(4000), 500, p, model.clone(), None).await;
    seed_tools(&c).await;
    let original = serde_json::to_value(c.messages()).unwrap();
    let db = rusqlite::Connection::open(dir.path().join("db.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER reject_summary BEFORE INSERT ON messages WHEN NEW.kind='summary' BEGIN SELECT RAISE(ABORT,'test'); END;").unwrap();
    assert!(c
        .compact(
            CompactionRequest::new(CompactionTrigger::Threshold),
            &CancellationToken::new()
        )
        .await
        .is_err());
    assert!(c.build_request(&[]).is_err()); // cannot issue requests with uncertain memory
    c.restore().await.unwrap();
    assert_eq!(original, serde_json::to_value(c.messages()).unwrap());
    assert!(c
        .records()
        .iter()
        .all(|r| r.tool_output_pruned_at.is_none()));
    let usage = LocalUsage((*store).clone())
        .session_usage(c.session_id())
        .await
        .unwrap();
    assert_eq!(usage.total_tokens, 110);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
}
#[tokio::test]
async fn latest_user_unresolved_batch_and_parallel_pairs_are_protected() {
    let model = Summarizer::new();
    let (_, _, c) = setup(Some(12_000), 500, policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old".repeat(1000)),
            vec![call("a"), call("b")].into(),
            tool("a", "success".into()),
            tool("b", "success".into()),
            ChatMessage::user("latest goal"),
            vec![call("pending")].into(),
        ],
    )
    .await;
    let result = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    let records = c.records();
    assert_eq!(records.len(), 3);
    assert!(records[0].summary);
    assert_eq!(records[1].message.content.first_text(), Some("latest goal"));
    assert_eq!(
        records[2].message.content.tool_calls()[0].call_id,
        "pending"
    );
    let req = &model.requests.lock().unwrap()[0].request;
    assert!(req.tools.as_ref().unwrap().is_empty());
    assert!(req
        .messages
        .iter()
        .all(|m| m.content.tool_calls().is_empty() && m.content.tool_responses().is_empty()));
    let history = req.messages[0].content.texts().join("\n");
    for id in ["a", "b"] {
        assert_eq!(history.matches(&format!("\"call_id\":\"{id}\"")).count(), 2);
    }
    assert!(!history.contains("pending"));
}
#[tokio::test]
async fn long_turn_can_compact_closed_batches_while_retaining_its_user_request() {
    let (_, _, c) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("only user request"),
            vec![call("a")].into(),
            tool("a", "large output".repeat(400)),
            ChatMessage::assistant("continue"),
        ],
    )
    .await;
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(c
        .messages()
        .iter()
        .any(|m| m.content.first_text() == Some("only user request")));
    assert!(c.records()[0].summary);
    assert_eq!(c.records().len(), 3);
}
#[tokio::test]
async fn chunking_has_one_final_commit_and_enforces_call_budget() {
    let mut p = policy();
    p.safety_margin = 100;
    let model = Summarizer::new();
    let (_, store, c) = setup(Some(2200), 200, p, model.clone(), None).await;
    let mut messages: Vec<_> = (0..5)
        .map(|i| ChatMessage::user(format!("old {i}:{}", "x".repeat(3000))))
        .collect();
    messages.push(ChatMessage::user("latest"));
    append(&c, messages).await;
    let mut limited = manual();
    limited.max_model_calls = 1;
    assert!(c.compact(limited, &CancellationToken::new()).await.is_err());
    assert!(!c.records().iter().any(|r| r.summary));
    let mut request = manual();
    request.max_model_calls = 8;
    let result = c.compact(request, &CancellationToken::new()).await.unwrap();
    assert!(result.compacted);
    assert!(model.requests.lock().unwrap().len() >= 4);
    let rows = store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(rows.iter().filter(|r| r.summary).count(), 1);
    assert_eq!(
        rows.iter()
            .filter(|r| r.status == MessageStatus::Compacted)
            .count(),
        5
    );
}
#[tokio::test]
async fn invalid_summary_is_accounted_and_never_committed() {
    let model = Arc::new(Summarizer {
        requests: Mutex::new(vec![]),
        empty: true,
    });
    let (_, store, c) = setup(Some(12_000), 500, policy(), model, None).await;
    append(
        &c,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    assert!(c
        .compact(manual(), &CancellationToken::new())
        .await
        .is_err());
    assert_eq!(c.records().len(), 2);
    assert_eq!(
        LocalUsage((*store).clone())
            .session_usage(c.session_id())
            .await
            .unwrap()
            .total_tokens,
        220
    );
}
#[tokio::test]
async fn post_hook_stop_reports_committed_state_and_is_not_dispatched_twice() {
    let hooks = Arc::new(support::Hooks::new(|inv, r| {
        if inv.event.kind() == HookEventKind::PostCompact {
            r.common.prevent_continuation = true;
            r.common.stop_reason = Some("stop after save".into());
        }
    }));
    let (_, _, c) = setup(
        Some(12_000),
        500,
        policy(),
        Summarizer::new(),
        Some(hooks.clone()),
    )
    .await;
    append(
        &c,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    let result = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    assert!(result.stop_reason.is_some());
    assert!(c.records()[0].summary);
    let seen = hooks.seen.lock().unwrap();
    assert_eq!(
        seen.iter()
            .filter(|e| **e == HookEventKind::PreCompact)
            .count(),
        1
    );
    assert_eq!(
        seen.iter()
            .filter(|e| **e == HookEventKind::PostCompact)
            .count(),
        1
    );
}

#[tokio::test]
async fn committed_summary_survives_reload_failure_and_still_runs_post_hook() {
    let hooks = Arc::new(support::Hooks::new(|invocation, result| {
        if invocation.event.kind() == HookEventKind::PostCompact {
            result.common.prevent_continuation = true;
            result.common.stop_reason = Some("another post-hook reason".into());
        }
    }));
    let (dir, store, context) = setup(
        Some(12_000),
        500,
        policy(),
        Summarizer::new(),
        Some(hooks.clone()),
    )
    .await;
    append(
        &context,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    // The transaction succeeds, but the immediately following reload reads an
    // invalid frozen prompt. This exercises the real SQLite acknowledgement.
    let db = rusqlite::Connection::open(dir.path().join("db.sqlite")).unwrap();
    db.execute_batch("CREATE TRIGGER break_reload AFTER INSERT ON messages WHEN NEW.kind='summary' BEGIN UPDATE sessions SET system_prompt=NULL WHERE session_id=NEW.session_id; END;").unwrap();
    let result = context
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    assert!(result
        .stop_reason
        .as_deref()
        .unwrap()
        .contains("active context restore failed"));
    assert!(context.build_request(&[]).is_err());
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|kind| **kind == HookEventKind::PostCompact)
            .count(),
        1
    );
    let persisted = store
        .read_messages(context.session_id(), MessageQuery::default())
        .await
        .unwrap();
    let summaries: Vec<_> = persisted
        .messages
        .iter()
        .filter(|message| message.summary)
        .collect();
    assert_eq!(summaries.len(), 1);
    assert!(summaries[0]
        .message
        .content
        .first_text()
        .unwrap()
        .contains("Decisions and completed work."));
    db.execute_batch("DROP TRIGGER break_reload; UPDATE sessions SET system_prompt='';")
        .unwrap();
    context.restore().await.unwrap();
    assert!(context.records()[0].summary);
    assert!(context.build_request(&[]).is_ok());
    assert!(
        !context
            .compact(
                CompactionRequest::new(CompactionTrigger::Threshold),
                &CancellationToken::new()
            )
            .await
            .unwrap()
            .compacted
    );
    assert_eq!(
        hooks
            .seen
            .lock()
            .unwrap()
            .iter()
            .filter(|kind| **kind == HookEventKind::PostCompact)
            .count(),
        1
    );
}
#[tokio::test]
async fn observed_usage_is_invalidated_by_tools_projection() {
    let (_, _, c) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    append(&c, vec![ChatMessage::user("x".repeat(2000))]).await;
    let before = c.build_request(&[]).unwrap();
    let mut response = StoredMessage::new(ChatMessage::assistant("response"));

    response.request_observation = Some(RequestObservation {
        model: "summary".into(),
        request: before.request,
        input_tokens: 50,
    });
    c.append(vec![response]).await.unwrap();
    let calibrated = c.build_request(&[]).unwrap().estimated_tokens;
    assert!(calibrated < 200);
    assert!(
        c.build_request(&[ToolDefinition::new("different")])
            .unwrap()
            .estimated_tokens
            > 600
    );
}
#[tokio::test]
async fn automatic_cooldown_and_unknown_window_do_not_claim_success() {
    let p = policy();
    let model = Summarizer::new();
    let (_, _, c) = setup(None, 500, p, model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    assert_eq!(c.build_request(&[]).unwrap().input_budget, None);
    assert!(c
        .compact(manual(), &CancellationToken::new())
        .await
        .is_err());
    assert!(model.requests.lock().unwrap().is_empty());
}

#[tokio::test]
async fn automatic_summary_failure_is_not_repeated_during_model_retries() {
    struct RetryingModel {
        summary: Arc<Summarizer>,
        streams: std::sync::atomic::AtomicUsize,
    }
    impl ModelProvider for RetryingModel {
        fn model_iden(&self) -> &str {
            "summary"
        }
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            self.summary.complete(request)
        }
        fn stream_events<'a>(
            &'a self,
            _: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            Box::pin(async move {
                if self
                    .streams
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst)
                    == 0
                {
                    let stream: ModelEventStream = Box::pin(futures_util::stream::once(async {
                        Err(ErrorKind::Provider {
                            name: "test",
                            message: "retry once".into(),
                        }
                        .into())
                    }));
                    Ok(stream)
                } else {
                    Ok(support::answer("done"))
                }
            })
        }
        fn recovery(&self, _: &YourAiError) -> ModelRecovery {
            ModelRecovery::Retry
        }
    }
    let model = Arc::new(RetryingModel {
        summary: Arc::new(Summarizer {
            requests: Mutex::new(vec![]),
            empty: true,
        }),
        streams: Default::default(),
    });
    let mut p = policy();
    p.advance_tokens = Some(3000);
    let (_, _, c) = setup(Some(4000), 500, p, model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old history ".repeat(200)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let agent = Agent::builder()
        .model(context_fixture::configured(model.clone(), Some(4000), 500))
        .context_manager(c.inner.clone())
        .agent_loop(Arc::new(yourai_harness::default_loop::DefaultLoop))
        .build();
    let output = agent.run(In::user_text("continue")).await.unwrap();
    assert_eq!(output.text, "done");
    assert_eq!(model.summary.requests.lock().unwrap().len(), 2);
    assert_eq!(model.streams.load(std::sync::atomic::Ordering::SeqCst), 2);
    assert!(c.records().iter().all(|m| !m.summary));
    assert_eq!(output.usage.unwrap().total_tokens, 223);
}

struct UncertainStore {
    inner: Arc<SqliteStore>,
    committed: Arc<tokio::sync::Notify>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}
impl SessionStore for UncertainStore {
    fn claim_session(
        &self,
        id: &SessionId,
        owner: &str,
        ttl: u64,
    ) -> Result<Arc<dyn SessionStore>, YourAiError> {
        self.inner.claim_session(id, owner, ttl)
    }
    fn renew_session(&self, ttl: u64) -> Result<(), YourAiError> {
        self.inner.renew_session(ttl)
    }
    fn release_session(&self) -> Result<(), YourAiError> {
        self.inner.release_session()
    }
    fn read_state(&self, id: &SessionId, key: &str) -> Result<Option<SessionState>, YourAiError> {
        self.inner.read_state(id, key)
    }
    fn save_state(
        &self,
        id: &SessionId,
        key: &str,
        revision: Option<i64>,
        value: serde_json::Value,
    ) -> Result<i64, YourAiError> {
        self.inner.save_state(id, key, revision, value)
    }
    fn append_output<'a>(
        &'a self,
        id: &'a SessionId,
        output: &'a str,
        offset: u64,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.inner.append_output(id, output, offset, bytes)
    }
    fn read_output<'a>(
        &'a self,
        id: &'a SessionId,
        output: &'a str,
        offset: u64,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<u8>, YourAiError>> {
        self.inner.read_output(id, output, offset, limit)
    }
    fn cleanup_outputs<'a>(
        &'a self,
        id: &'a SessionId,
        before: i64,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.inner.cleanup_outputs(id, before)
    }
    fn read_tasks(&self, id: &SessionId) -> Result<Vec<Task>, YourAiError> {
        self.inner.read_tasks(id)
    }
    fn save_tasks(&self, id: &SessionId, tasks: Vec<Task>) -> Result<Vec<Task>, YourAiError> {
        self.inner.save_tasks(id, tasks)
    }

    fn initialize_system<'a>(
        &'a self,
        id: &'a SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        self.inner.initialize_system(id, system)
    }

    fn read_messages<'a>(
        &'a self,
        id: &'a SessionId,
        q: MessageQuery,
    ) -> BoxFuture<'a, Result<MessagePage, YourAiError>> {
        self.reads.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.read_messages(id, q)
    }
    fn append_messages<'a>(
        &'a self,
        id: &'a SessionId,
        m: Vec<StoredMessage>,
    ) -> BoxFuture<'a, Result<Vec<StoredMessage>, YourAiError>> {
        self.inner.append_messages(id, m)
    }
    fn save_summary<'a>(
        &'a self,
        id: &'a SessionId,
        sources: Vec<String>,
        summary: StoredMessage,
        progress: std::sync::Arc<std::sync::atomic::AtomicU8>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.inner
                .save_summary(id, sources, summary, progress)
                .await?;
            self.committed.notify_one();
            std::future::pending().await
        })
    }
    fn create_session<'a>(
        &'a self,
        id: SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.inner.create_session(id, system)
    }
    fn load_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.inner.load_session(id)
    }
    fn save_session<'a>(&'a self, m: &'a SessionMeta) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.inner.save_session(m)
    }
    fn list_sessions(&self) -> BoxFuture<'_, Result<Vec<SessionMeta>, YourAiError>> {
        self.inner.list_sessions()
    }
    fn delete_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.inner.delete_session(id)
    }
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        self.inner.fork_session(id)
    }
}
#[tokio::test]
async fn cancelled_uncertain_commit_blocks_reads_and_next_append_recovers_first() {
    let (_dir, store, initial) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    append(
        &initial,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    let id = initial.session_id().clone();
    let committed = Arc::new(tokio::sync::Notify::new());
    let mut services = ContextServices::new(&id);
    services.policy = policy();
    services.store = Some(Arc::new(UncertainStore {
        inner: store,
        committed: committed.clone(),
        reads: Default::default(),
    }));
    let c = DefaultContext::new(
        id,
        context_fixture::configured(Summarizer::new(), Some(12_000), 500),
        services,
    );
    c.restore().await.unwrap();
    let cancel = CancellationToken::new();
    let ctx = c.clone();
    let token = cancel.clone();
    let work = tokio::spawn(async move { ctx.compact(manual(), &token).await });
    committed.notified().await;
    cancel.cancel();
    let error = work.await.unwrap().unwrap_err();
    assert!(error.to_string().contains("summary committed"), "{error}");
    assert!(c.build_request(&[]).is_err());
    append(&c, vec![ChatMessage::user("after cancellation")]).await;
    assert!(c.records()[0].summary);
    assert_eq!(c.records().len(), 3);
    assert!(c.build_request(&[]).is_ok());
}

#[tokio::test]
async fn latest_user_protection_uses_persisted_origin_not_text_prefixes() {
    let (_, _, c) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old".repeat(1000)),
            ChatMessage::user("[Runtime context]\nthis is actually the user's request"),
        ],
    )
    .await;
    c.append(vec![
        StoredMessage::runtime_context("[Runtime context]\nextra hook context"),
        StoredMessage::new(ChatMessage::assistant("continue")),
    ])
    .await
    .unwrap();
    c.restore().await.unwrap();
    assert!(c.records()[2].runtime_context);
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(c.messages().iter().any(|m| m.content.first_text()
        == Some("[Runtime context]\nthis is actually the user's request")));
}

#[tokio::test]
async fn successful_append_uses_committed_rows_without_reloading_history() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let (_dir, store, initial) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let id = initial.session_id().clone();
    let mut services = ContextServices::new(&id);
    services.store = Some(Arc::new(UncertainStore {
        inner: store.clone(),
        committed: Default::default(),
        reads: reads.clone(),
    }));
    let c = DefaultContext::new(
        id,
        context_fixture::configured(Summarizer::new(), Some(12_000), 500),
        services,
    );
    c.restore().await.unwrap();
    let baseline = reads.load(Ordering::SeqCst);
    let row = StoredMessage::new(ChatMessage::user("one"));
    c.append(vec![row.clone()]).await.unwrap();
    c.append(vec![row]).await.unwrap();
    c.append(vec![StoredMessage::new(ChatMessage::assistant("two"))])
        .await
        .unwrap();
    assert_eq!(reads.load(Ordering::SeqCst), baseline);
    assert_eq!(
        c.records().iter().map(|r| r.seq).collect::<Vec<_>>(),
        vec![1, 2]
    );
    c.restore().await.unwrap();
    assert_eq!(c.records().len(), 2);
}

struct MediaSummary(Arc<Summarizer>);
impl ModelProvider for MediaSummary {
    fn model_iden(&self) -> &str {
        "media-test"
    }
    fn media_tokens(&self, _: &ContentPart) -> Result<u64, YourAiError> {
        Ok(900)
    }
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        self.0.complete(r)
    }
    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        self.0.stream_events(r)
    }
}
#[tokio::test]
async fn compact_uses_message_content_without_opening_media_locations() {
    let model = Summarizer::new();
    let (_dir, _store, c) = setup(
        Some(12_000),
        500,
        policy(),
        Arc::new(MediaSummary(model.clone())),
        None,
    )
    .await;
    let media = |url| ContentPart::Binary(Binary::from_url("image/png", url, None));
    let old = "file:///does-not-exist/old.png";
    let current = "file:///does-not-exist/current.png";
    append(
        &c,
        vec![
            ChatMessage::user(MessageContent::from_parts(vec![
                ContentPart::from_text("previous observations ".repeat(100)),
                media(old),
            ])),
            ChatMessage::user(MessageContent::from_parts(vec![
                ContentPart::from_text("current question"),
                media(current),
            ])),
        ],
    )
    .await;
    let result = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    let requests = model.requests.lock().unwrap();
    let summary = &requests[0].request;
    assert!(summary.messages.iter().all(|m| m
        .content
        .parts()
        .iter()
        .all(|p| !matches!(p, ContentPart::Binary(_)))));
    let text = serde_json::to_string(summary).unwrap();
    assert!(text.contains(old));
    assert!(!text.contains(current));
    assert!(c
        .build_request(&[])
        .unwrap()
        .request
        .messages
        .last()
        .unwrap()
        .content
        .parts()
        .iter()
        .any(|p| matches!(p, ContentPart::Binary(_))));
}

/// Regression for the attachment feature: `GenaiModel` must implement
/// `media_tokens`. Before it did, the fail-closed trait default made
/// `DefaultContext::build_request` reject every request whose history
/// contained a Binary part ("media budgeting/capability is not configured").
#[tokio::test]
async fn binary_attachment_builds_request_with_genai_model() {
    use base64::Engine as _;
    let model =
        yourai_harness::GenaiModel::new(genai::Client::builder().build().unwrap(), "test-model");
    let (_dir, _store, c) = setup(Some(12_000), 500, policy(), Arc::new(model), None).await;
    // Blank 2000x2000 PNG — compresses to a tiny payload but keeps full
    // dimensions for the estimator.
    let img = image::DynamicImage::new_rgb8(2000, 2000);
    let mut buf = Vec::new();
    img.to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .unwrap();
    let data = base64::engine::general_purpose::STANDARD.encode(&buf);
    append(
        &c,
        vec![ChatMessage::user(MessageContent::from_parts(vec![
            ContentPart::from_text("describe this"),
            ContentPart::from_binary_base64("image/png", data.as_str(), None),
        ]))],
    )
    .await;
    let request = c.build_request(&[]).unwrap();
    // 2000x2000 -> max(anthropic 1568^2/750 = 3279, openai 4 tiles = 765).
    assert!(
        request.estimated_tokens >= 3279,
        "image tokens must be counted: {}",
        request.estimated_tokens
    );
    // ... and the Binary part survives into the outgoing request.
    assert!(request
        .request
        .messages
        .last()
        .unwrap()
        .content
        .parts()
        .iter()
        .any(|p| matches!(p, ContentPart::Binary(_))));
}

#[tokio::test(start_paused = true)]
async fn compaction_has_no_implicit_deadline_but_honors_explicit_deadline_and_cancel() {
    use std::time::{Duration, Instant};
    struct SlowPreCompact;
    impl HookManager for SlowPreCompact {
        // This fixed-response fixture does not support dynamic registration.
        fn register<'a>(
            &'a self,
            _: yourai_core::hooks::NativeHookRegistration,
        ) -> yourai_core::BoxFuture<'a, Result<(), yourai_core::YourAiError>> {
            panic!("registration is not used by this test hook")
        }
        fn unregister<'a>(
            &'a self,
            _: &'a str,
        ) -> yourai_core::BoxFuture<'a, Result<bool, yourai_core::YourAiError>> {
            panic!("unregistration is not used by this test hook")
        }
        fn handler_ids<'a>(&'a self) -> yourai_core::BoxFuture<'a, Vec<String>> {
            panic!("registry inspection is not used by this test hook")
        }

        fn dispatch<'a>(
            &'a self,
            i: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
            Box::pin(async move {
                if matches!(i.event, HookEvent::PreCompact { .. }) {
                    tokio::time::sleep(Duration::from_secs(121)).await;
                }
                Ok(HookDispatchResult::empty(i.event.kind()))
            })
        }
    }
    for mode in ["unlimited", "deadline", "cancel"] {
        let id = SessionId::new();
        let mut services = ContextServices::new(&id);
        services.policy = policy();
        services.hooks = Some(Arc::new(SlowPreCompact));
        let context = DefaultContext::new(
            id,
            context_fixture::configured(Summarizer::new(), Some(12_000), 500),
            services,
        );
        append(
            &context,
            vec![
                ChatMessage::user("old task".repeat(500)),
                ChatMessage::assistant("completed"),
                ChatMessage::user("next"),
            ],
        )
        .await;
        let mut request = manual();
        let cancel = CancellationToken::new();
        if mode == "deadline" {
            request.deadline = Some(Instant::now());
        }
        if mode == "cancel" {
            cancel.cancel();
        }
        let result = context.compact(request, &cancel).await;
        match mode {
            "unlimited" => {
                result.unwrap();
                assert!(context.records().iter().any(|r| r.summary));
            }
            "deadline" => assert!(matches!(
                result.unwrap_err(),
                YourAiError::Aborted(AbortReason::DeadlineExceeded)
            )),
            _ => assert!(matches!(
                result.unwrap_err(),
                YourAiError::Aborted(AbortReason::Cancelled)
            )),
        }
    }
}

#[tokio::test]
async fn pre_compact_can_append_authoritative_context_without_holding_context_lock() {
    struct AppendContext(Arc<dyn ContextManager>);
    impl HookManager for AppendContext {
        // This fixed-response fixture does not support dynamic registration.
        fn register<'a>(
            &'a self,
            _: yourai_core::hooks::NativeHookRegistration,
        ) -> yourai_core::BoxFuture<'a, Result<(), yourai_core::YourAiError>> {
            panic!("registration is not used by this test hook")
        }
        fn unregister<'a>(
            &'a self,
            _: &'a str,
        ) -> yourai_core::BoxFuture<'a, Result<bool, yourai_core::YourAiError>> {
            panic!("unregistration is not used by this test hook")
        }
        fn handler_ids<'a>(&'a self) -> yourai_core::BoxFuture<'a, Vec<String>> {
            panic!("registry inspection is not used by this test hook")
        }

        fn dispatch<'a>(
            &'a self,
            invocation: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
            Box::pin(async move {
                if invocation.event.kind() == HookEventKind::PreCompact {
                    let mut row = StoredMessage::runtime_context("Keep the exact new rule.");
                    row.context_key = Some("rules".into());
                    self.0.append(vec![row]).await?;
                }
                Ok(HookDispatchResult::empty(invocation.event.kind()))
            })
        }
    }
    let model = Summarizer::new();
    let (_, _, c) = setup(Some(12_000), 500, policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old history ".repeat(300)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let compact = Compactor::new(
        c.inner.clone(),
        context_fixture::configured(model.clone(), Some(12_000), 500),
        Some(Arc::new(AppendContext(c.inner.clone()))),
        None,
        BaseInput::new(c.session_id().as_str(), ""),
    );
    let result = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        compact.exec(manual(), &CancellationToken::new(), None),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(result.compacted);
    assert!(c
        .records()
        .iter()
        .any(|m| m.context_key.as_deref() == Some("rules")
            && m.message.content.first_text() == Some("Keep the exact new rule.")));
    assert!(
        !serde_json::to_string(&model.requests.lock().unwrap()[0].request)
            .unwrap()
            .contains("Keep the exact new rule.")
    );
}

#[tokio::test]
async fn concurrent_append_discards_stale_summary_without_losing_or_archiving_messages() {
    struct SuspendedSummary {
        model: Arc<Summarizer>,
        entered: tokio::sync::Notify,
        release: tokio::sync::Notify,
    }
    impl ModelProvider for SuspendedSummary {
        fn model_iden(&self) -> &str {
            "summary"
        }
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async move {
                self.entered.notify_one();
                self.release.notified().await;
                self.model.complete(request).await
            })
        }
        fn stream_events<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.model.stream_events(request)
        }
    }
    let model = Arc::new(SuspendedSummary {
        model: Summarizer::new(),
        entered: Default::default(),
        release: Default::default(),
    });
    let (_, store, c) = setup(Some(12_000), 500, policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old ".repeat(600)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let context = c.clone();
    let pending =
        tokio::spawn(async move { context.compact(manual(), &CancellationToken::new()).await });
    model.entered.notified().await;
    append(&c, vec![ChatMessage::user("new message while summarizing")]).await;
    model.release.notify_one();
    let error = pending.await.unwrap().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("context changed during compaction"),
        "{error}"
    );
    assert_eq!(c.records().len(), 3);
    assert!(c.records().iter().all(|m| !m.summary));
    c.restore().await.unwrap();
    assert_eq!(c.records().len(), 3);
    assert_eq!(
        LocalUsage((*store).clone())
            .session_usage(c.session_id())
            .await
            .unwrap()
            .total_tokens,
        110
    );
}

#[tokio::test]
async fn repeated_compaction_updates_an_explicit_summary_with_its_own_output_budget() {
    let model = Summarizer::new();
    let (_, _, c) = setup(Some(100_000), 32_000, policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old goal ".repeat(500)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    append(
        &c,
        vec![
            ChatMessage::assistant("new work ".repeat(500)),
            ChatMessage::user("updated task"),
        ],
    )
    .await;
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 2);
    for request in requests.iter() {
        assert_eq!(request.options.max_tokens, Some(8000));
        assert!(request.request.tools.as_ref().unwrap().is_empty());
        assert!(request
            .request
            .system
            .as_deref()
            .unwrap()
            .contains("Completed and verified"));
    }
    let text = requests[1].request.messages[0]
        .content
        .first_text()
        .unwrap();
    let (previous, history) = text.split_once("</previous-summary>").unwrap();
    assert!(previous.contains("Decisions and completed work."));
    assert!(!history.contains("Decisions and completed work."));
    assert!(history.contains("new work"));
    assert_eq!(c.records().iter().filter(|m| m.summary).count(), 1);
}

#[tokio::test]
async fn current_keyed_instructions_survive_compaction_restore_and_fork_verbatim() {
    let model = Summarizer::new();
    let (_, store, c) = setup(Some(12_000), 500, policy(), model.clone(), None).await;
    let keyed = |text: &str| {
        let mut row = StoredMessage::runtime_context(text);
        row.context_key = Some("instructions:/project/AGENTS.md".into());
        row
    };
    c.append(vec![
        keyed("obsolete instruction"),
        StoredMessage::new(ChatMessage::user("old history ".repeat(300))),
        keyed("current exact instruction"),
        StoredMessage::new(ChatMessage::user("current task")),
    ])
    .await
    .unwrap();
    let before = serde_json::to_string(&c.build_request(&[]).unwrap().request).unwrap();
    assert!(!before.contains("obsolete instruction"));
    assert!(before.contains("current exact instruction"));
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    c.restore().await.unwrap();
    let after = serde_json::to_string(&c.build_request(&[]).unwrap().request).unwrap();
    assert!(after.contains("current exact instruction"));
    assert!(!after.contains("obsolete instruction"));
    let summary_request =
        serde_json::to_string(&model.requests.lock().unwrap()[0].request).unwrap();
    assert!(!summary_request.contains("current exact instruction"));
    assert!(!summary_request.contains("obsolete instruction"));
    let child = store.fork_session(c.session_id()).await.unwrap();
    let mut services = ContextServices::new(&child);
    services.store = Some(store);
    services.policy = policy();
    let fork = DefaultContext::new(
        child,
        context_fixture::configured(model, Some(12_000), 500),
        services,
    );
    fork.restore().await.unwrap();
    assert_eq!(
        serde_json::to_value(c.build_request(&[]).unwrap().request).unwrap(),
        serde_json::to_value(fork.build_request(&[]).unwrap().request).unwrap()
    );
}

#[tokio::test]
async fn old_unkeyed_workspace_instructions_are_preserved_after_restore() {
    let (_, _, c) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    c.append(vec![
        StoredMessage::runtime_context("[Instructions: /project/AGENTS.md]\nlegacy rule"),
        StoredMessage::new(ChatMessage::user("old history ".repeat(300))),
        StoredMessage::new(ChatMessage::user("current task")),
    ])
    .await
    .unwrap();
    c.restore().await.unwrap();
    assert_eq!(
        c.records()[0].context_key.as_deref(),
        Some("instructions:/project/AGENTS.md")
    );
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(c.records().iter().any(|m| m.message.content.first_text()
        == Some("[Instructions: /project/AGENTS.md]\nlegacy rule")));
}

#[tokio::test]
async fn summary_bounds_its_own_tool_material_and_preserves_the_original_and_retrieval_path() {
    let model = Summarizer::new();
    let (_, store, c) = setup(Some(4000), 500, policy(), model.clone(), None).await;
    let original =
        json!({"ok":true,"output_paths":["/project/output.txt"],"output":"large ".repeat(100_000)})
            .to_string();
    append(
        &c,
        vec![
            ChatMessage::user("old task"),
            vec![call("large")].into(),
            tool("large", original.clone()),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    c.compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    {
        let requests = model.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let text = requests[0].request.messages[0]
            .content
            .first_text()
            .unwrap();
        assert!(text.contains("/project/output.txt"));
        assert!(!text.contains(&"large ".repeat(1000)));
    }
    let archive = store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap();
    assert!(archive
        .messages
        .iter()
        .flat_map(|m| m.message.content.tool_responses())
        .any(|response| response.content == original));
}

#[tokio::test]
async fn pinned_summary_model_uses_its_capacity_but_admits_against_the_main_model() {
    let main = Summarizer::new();
    let summary = Summarizer::new();
    let (_, _, c) = setup(Some(4000), 500, policy(), main.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("older work ".repeat(2000)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let agent = Agent::builder()
        .agent_loop(Arc::new(yourai_harness::default_loop::DefaultLoop))
        .model(context_fixture::configured(main.clone(), Some(4000), 500))
        .compaction_model(context_fixture::configured(
            summary.clone(),
            Some(100_000),
            8000,
        ))
        .context_manager(c.inner.clone())
        .build();
    let execution = Compactor::from_snapshot(&agent.snapshot().unwrap(), None).unwrap();
    let result = execution
        .exec(manual(), &CancellationToken::new(), None)
        .await
        .unwrap();
    assert!(result.compacted);
    assert!(result.tokens_after <= 3400);
    assert!(main.requests.lock().unwrap().is_empty());
    let requests = summary.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert!(requests[0].options.max_tokens.unwrap() > 500);
    assert!(requests[0].request.messages[0]
        .content
        .first_text()
        .unwrap()
        .contains("older work"));
}

#[tokio::test]
async fn summary_shortens_tool_logs_and_keeps_both_ends_and_original_history() {
    let model = Summarizer::new();
    let (_, store, c) = setup(Some(4000), 500, policy(), model.clone(), None).await;
    let output = format!(
        "IMPORTANT_HEAD\n{}\nFINAL_EXIT_1",
        "log line\n".repeat(3000)
    );
    append(
        &c,
        vec![
            ChatMessage::user("old goal"),
            vec![call("logs")].into(),
            tool("logs", output.clone()),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let result = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    {
        let requests = model.requests.lock().unwrap();
        let history = requests[0].request.messages[0]
            .content
            .first_text()
            .unwrap();
        assert!(history.contains("IMPORTANT_HEAD"));
        assert!(history.contains("FINAL_EXIT_1"));
        assert!(history.contains("Tool output shortened for summary"));
        assert!(!history.contains(&"log line\\n".repeat(500)));
    }
    assert!(store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap()
        .messages
        .iter()
        .flat_map(|m| m.message.content.tool_responses())
        .any(|r| r.content == output));
}

#[tokio::test]
async fn oversized_old_history_is_reported_and_archived_only_after_a_valid_summary() {
    let model = Summarizer::new();
    let (_, store, c) = setup(Some(4000), 500, policy(), model, None).await;
    let oversized = "older oversized material ".repeat(2000);
    append(
        &c,
        vec![
            ChatMessage::user(oversized.clone()),
            ChatMessage::user("later goal ".repeat(300)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    let result = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(result.compacted);
    assert!(result
        .notices
        .iter()
        .any(|s| s.contains("omitted old history")));
    assert!(c
        .records()
        .iter()
        .find(|m| m.summary)
        .unwrap()
        .message
        .content
        .first_text()
        .unwrap()
        .contains("History omission"));
    let rows = store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert!(rows.iter().any(
        |r| r.message.content.first_text() == Some(oversized.as_str())
            && r.status == MessageStatus::Compacted
    ));
    // The newest selected group must never be discarded just to claim success.
    append(
        &c,
        vec![
            ChatMessage::assistant("newest oversized material ".repeat(2000)),
            ChatMessage::user("latest task"),
        ],
    )
    .await;
    let before = c.records().iter().map(|r| r.id.clone()).collect::<Vec<_>>();
    assert!(c
        .compact(manual(), &CancellationToken::new())
        .await
        .is_err());
    assert_eq!(
        c.records().iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        before
    );
}

#[tokio::test]
async fn rejected_summary_shrinks_batches_and_commits_one_checkpoint() {
    struct RejectOnce {
        requests: Mutex<Vec<ModelRequest>>,
        summary: Arc<Summarizer>,
    }
    impl ModelProvider for RejectOnce {
        fn model_iden(&self) -> &str {
            "summary"
        }
        fn classify_error(&self, _: &YourAiError) -> ModelErrorClass {
            ModelErrorClass::ContextOverflow
        }
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async move {
                let first = {
                    let mut seen = self.requests.lock().unwrap();
                    let first = seen.is_empty();
                    seen.push(request.clone());
                    first
                };
                if first {
                    return Err(ErrorKind::Provider {
                        name: "test",
                        message: "context overflow".into(),
                    }
                    .into());
                }
                self.summary.complete(request).await
            })
        }
        fn stream_events<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.summary.stream_events(request)
        }
    }
    let model = Arc::new(RejectOnce {
        requests: Mutex::new(vec![]),
        summary: Summarizer::new(),
    });
    let (_, store, c) = setup(Some(4000), 500, policy(), model.clone(), None).await;
    let mut messages: Vec<_> = (0..6)
        .map(|i| ChatMessage::user(format!("old {i}:{}", "x".repeat(2500))))
        .collect();
    messages.push(ChatMessage::user("latest task"));
    append(&c, messages).await;
    let request = manual();
    let calls = request.calls.clone();
    let result = c.compact(request, &CancellationToken::new()).await.unwrap();
    assert!(result.compacted);
    assert!(result.notices.is_empty());
    {
        let requests = model.requests.lock().unwrap();
        assert!(
            requests[1].request.messages[0]
                .content
                .first_text()
                .unwrap()
                .len()
                < requests[0].request.messages[0]
                    .content
                    .first_text()
                    .unwrap()
                    .len()
        );
        assert_eq!(
            calls.load(std::sync::atomic::Ordering::Acquire) as usize,
            requests.len()
        );
    }
    let rows = store
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap()
        .messages;
    assert_eq!(rows.iter().filter(|r| r.summary).count(), 1);
    assert_eq!(
        rows.iter()
            .filter(|r| r.status == MessageStatus::Compacted)
            .count(),
        6
    );
}

#[tokio::test]
async fn summary_format_retry_keeps_original_history_until_valid_reply() {
    struct InvalidOnce(Arc<Summarizer>);
    impl ModelProvider for InvalidOnce {
        fn model_iden(&self) -> &str {
            "summary"
        }
        fn complete<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
            Box::pin(async move {
                let first = self.0.requests.lock().unwrap().is_empty();
                let mut reply = self.0.complete(request).await?;
                if first {
                    reply.content = MessageContent::from_text("unstructured answer");
                }
                Ok(reply)
            })
        }
        fn stream_events<'a>(
            &'a self,
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.0.stream_events(request)
        }
    }
    let inner = Summarizer::new();
    let (_, store, c) = setup(
        Some(12_000),
        500,
        policy(),
        Arc::new(InvalidOnce(inner.clone())),
        None,
    )
    .await;
    append(
        &c,
        vec![
            ChatMessage::user("old goal ".repeat(500)),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    assert!(
        c.compact(manual(), &CancellationToken::new())
            .await
            .unwrap()
            .compacted
    );
    {
        let requests = inner.requests.lock().unwrap();
        assert_eq!(requests.len(), 2);
        assert_eq!(
            serde_json::to_value(&requests[0].request.messages).unwrap(),
            serde_json::to_value(&requests[1].request.messages).unwrap()
        );
        assert!(requests[1]
            .request
            .system
            .as_ref()
            .unwrap()
            .contains("previous attempt"));
    }
    assert_eq!(c.records().iter().filter(|r| r.summary).count(), 1);
    assert_eq!(
        LocalUsage((*store).clone())
            .session_usage(c.session_id())
            .await
            .unwrap()
            .total_tokens,
        220
    );
}

#[tokio::test]
async fn recent_retention_prefers_a_whole_user_exchange() {
    let mut p = policy();
    p.keep_recent_tokens = 500;
    let model = Summarizer::new();
    let (_, _, c) = setup(Some(12_000), 500, p, model.clone(), None).await;
    let recent_user = "recent request ".repeat(150);
    let recent_reply = "recent response ".repeat(25);
    append(
        &c,
        vec![
            ChatMessage::user("older task ".repeat(500)),
            ChatMessage::assistant("old reply"),
            ChatMessage::user(recent_user.clone()),
            ChatMessage::assistant(recent_reply.clone()),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    assert!(
        c.compact(manual(), &CancellationToken::new())
            .await
            .unwrap()
            .compacted
    );
    let records = c.records();
    assert!(records
        .iter()
        .any(|r| r.message.content.first_text() == Some(recent_user.as_str())));
    assert!(records
        .iter()
        .any(|r| r.message.content.first_text() == Some(recent_reply.as_str())));
    assert!(
        !serde_json::to_string(&model.requests.lock().unwrap()[0].request)
            .unwrap()
            .contains("recent request")
    );
}

#[tokio::test]
async fn restore_loads_only_active_bodies_and_preserves_archived_identities() {
    let (dir, store, c) = setup(Some(12_000), 500, policy(), Summarizer::new(), None).await;
    let marker = "[Runtime event saved] handled";
    append(
        &c,
        vec![
            ChatMessage::user("old task ".repeat(500)),
            vec![call("archived-call")].into(),
            tool("archived-call", "done".into()),
        ],
    )
    .await;
    c.append(vec![StoredMessage::runtime_context(marker)])
        .await
        .unwrap();
    append(&c, vec![ChatMessage::user("current task")]).await;
    assert!(
        c.compact(manual(), &CancellationToken::new())
            .await
            .unwrap()
            .compacted
    );
    let index = store.history_index(c.session_id()).await.unwrap();
    assert!(index.call_ids.contains(&"archived-call".to_owned()));
    assert!(index.result_ids.contains(&"archived-call".to_owned()));
    let db = rusqlite::Connection::open(dir.path().join("db.sqlite")).unwrap();
    // Archived payloads must not be decoded while restoring the active context.
    db.execute(
        "UPDATE messages SET format_version=99 WHERE status='compacted'",
        [],
    )
    .unwrap();
    c.restore().await.unwrap();
    assert!(c.contains_tool_call("archived-call"));
    assert!(c.contains_context_marker(marker));
    assert_eq!(c.last_sequence(), index.last_seq);
    let before = c.records().len();
    append(&c, vec![tool("archived-call", "done".into())]).await;
    assert_eq!(c.records().len(), before);
}
