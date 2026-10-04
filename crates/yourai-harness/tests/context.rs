#[path = "support/context.rs"]
mod context_fixture;
use context_fixture::{configured, ContextServices, MemoryContext};
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
                    "Decisions and completed work."
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
        advance_tokens: 2000,
        keep_recent_tokens: 0,
        summary_min_savings: 10,
        ..Default::default()
    }
}
async fn setup(
    p: ContextPolicy,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> (TempDir, Arc<SqliteStore>, Arc<MemoryContext>) {
    setup_model(p, configured(model, Some(12_000), 500), hooks).await
}
async fn setup_model(
    p: ContextPolicy,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
) -> (TempDir, Arc<SqliteStore>, Arc<MemoryContext>) {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(SqliteStore::open(&dir.path().join("db.sqlite")).unwrap());
    let id = store.create_session("").await.unwrap().id;
    let mut services = ContextServices::new(&id);
    services.store = Some(store.clone());
    services.policy = p;
    services.hooks = hooks;
    services.usage = Some(Arc::new(LocalUsage((*store).clone())));
    let context = MemoryContext::new(id, model, services);
    context.restore().await.unwrap();
    (dir, store, context)
}
async fn append(c: &MemoryContext, messages: Vec<ChatMessage>) {
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
async fn seed_tools(c: &MemoryContext) {
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
    let mut response = StoredMessage::new(ChatMessage::assistant("read both outputs"));
    response.model_response = true;
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
        if let HookPointOutcome::Generic(outcome) = &mut result.outcome {
            outcome
                .additional_contexts
                .push("compaction context".into());
        }
    }));
    let model = Summarizer::new();
    let (_dir, _store, history) = setup(policy(), model.clone(), None).await;
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
        .model(history.execution.model().clone())
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
async fn projection_is_bounded_valid_json_and_original_is_preserved() {
    let mut p = policy();
    p.tool_output_chars = 512;
    let (_dir, _store, c) = setup(p, Summarizer::new(), None).await;
    let original = json!({"ok":true,"output":"你好🌍".repeat(1000)}).to_string();
    append(
        &c,
        vec![vec![call("a")].into(), tool("a", original.clone())],
    )
    .await;
    let projected = c.build_request(&[]).unwrap();
    let text = &projected.request.messages[1].content.tool_responses()[0].content;
    assert!(text.chars().count() <= 512);
    assert!(serde_json::from_str::<serde_json::Value>(text).is_ok());
    assert_eq!(
        c.records()[1].message.content.tool_responses()[0].content,
        original
    );
}
#[tokio::test]
async fn prune_only_avoids_model_hooks_and_survives_restore_and_fork() {
    let mut p = policy();
    p.advance_tokens = 2200;
    p.prune_enabled = true;
    p.prune_growth = 0;
    p.prune_min_savings = 50;
    let model = Summarizer::new();
    let hooks = Arc::new(support::Hooks::new(|_, _| {}));
    let (_dir, store, c) = setup_model(
        p.clone(),
        configured(model.clone(), Some(4000), 500),
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
    assert_eq!(result.action, CompactAction::Pruned);
    assert!(model.requests.lock().unwrap().is_empty());
    assert!(hooks.seen.lock().unwrap().is_empty());
    let active = c.records();
    assert!(active[2].tool_output_pruned_at.is_some());
    assert!(active[3].tool_output_pruned_at.is_none());
    assert!(active[2].message.content.tool_responses()[0]
        .content
        .contains(&"x".repeat(6000)));
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
    let fork = MemoryContext::new(child, configured(model, Some(4000), 500), services);
    fork.restore().await.unwrap();
    assert_eq!(
        expected,
        serde_json::to_value(fork.build_request(&[]).unwrap().request).unwrap()
    );
}
#[tokio::test]
async fn failed_combined_commit_keeps_pruning_and_summary_unapplied_but_accounts_usage() {
    let mut p = policy();
    p.advance_tokens = 3300;
    p.prune_enabled = true;
    p.prune_growth = 0;
    p.prune_min_savings = 50;
    let model = Summarizer::new();
    let (dir, store, c) = setup_model(p, configured(model.clone(), Some(4000), 500), None).await;
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
    let (_, _, c) = setup(policy(), model.clone(), None).await;
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
    assert_eq!(result.action, CompactAction::Summarized);
    let records = c.records();
    assert_eq!(records.len(), 3);
    assert!(records[0].summary);
    assert_eq!(records[1].message.content.first_text(), Some("latest goal"));
    assert_eq!(
        records[2].message.content.tool_calls()[0].call_id,
        "pending"
    );
    let req = &model.requests.lock().unwrap()[0].request;
    // Historical tool pairs are serialized as data, not executable tool messages.
    assert!(req
        .messages
        .iter()
        .all(|m| m.content.tool_calls().is_empty() && m.content.tool_responses().is_empty()));
    let text = req
        .messages
        .iter()
        .flat_map(|m| m.content.texts())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("success"));
    assert!(text.contains("call_id"));
    assert!(text.contains("Historical records"));
}
#[tokio::test]
async fn long_turn_can_compact_closed_batches_while_retaining_its_user_request() {
    let (_, _, c) = setup(policy(), Summarizer::new(), None).await;
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
    let (_, store, c) = setup_model(p, configured(model.clone(), Some(2200), 200), None).await;
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
    assert_eq!(result.action, CompactAction::Summarized);
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
    let (_, store, c) = setup(policy(), model, None).await;
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
        110
    );
}
#[tokio::test]
async fn oversized_checkpoint_is_rejected_without_replacing_history() {
    let model = Summarizer::new();
    let (_, store, c) = setup(policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old work ".repeat(500)),
            ChatMessage::user("current request"),
        ],
    )
    .await;
    let original_ids: Vec<_> = c.records().iter().map(|r| r.id.clone()).collect();
    let mut options = manual();
    options.target_tokens = Some(1);
    let result = c.compact(options, &CancellationToken::new()).await;
    assert!(result.unwrap_err().to_string().contains("summary exceeds"));
    assert_eq!(
        c.records().iter().map(|r| r.id.clone()).collect::<Vec<_>>(),
        original_ids
    );
    assert_eq!(
        model.requests.lock().unwrap()[0].options.max_tokens,
        Some(1)
    );
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
async fn post_hook_stop_reports_committed_state_and_is_not_dispatched_twice() {
    let hooks = Arc::new(support::Hooks::new(|inv, r| {
        if inv.event.kind() == HookEventKind::PostCompact {
            r.common.prevent_continuation = true;
            r.common.stop_reason = Some("stop after save".into());
        }
    }));
    let (_, _, c) = setup(policy(), Summarizer::new(), Some(hooks.clone())).await;
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
    assert_eq!(result.action, CompactAction::Summarized);
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
async fn observed_usage_is_invalidated_by_tools_projection() {
    let (_, _, c) = setup(policy(), Summarizer::new(), None).await;
    append(&c, vec![ChatMessage::user("x".repeat(2000))]).await;
    let before = c.build_request(&[]).unwrap();
    let mut response = StoredMessage::new(ChatMessage::assistant("response"));
    response.model_response = true;
    response.request_observation = Some(RequestObservation {
        model: "summary".into(),
        request: before.request,
        input_tokens: 50,
    });
    c.append(vec![response]).await.unwrap();
    let calibrated = c.build_request(&[]).unwrap().estimated_tokens;
    assert!(calibrated < 200);
    assert!(
        c.build_request(&[Tool::new("different")])
            .unwrap()
            .estimated_tokens
            > 600
    );
}
#[tokio::test]
async fn automatic_cooldown_and_unknown_window_do_not_claim_success() {
    let p = policy();
    let model = Summarizer::new();
    let (_, _, c) = setup_model(p, configured(model.clone(), None, 500), None).await;
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
async fn automatic_failure_is_not_repeated_for_the_same_request() {
    let mut p = policy();
    p.advance_tokens = 3000;
    let model = Arc::new(Summarizer {
        requests: Mutex::new(vec![]),
        empty: true,
    });
    let (_, _, c) = setup_model(p, configured(model.clone(), Some(4000), 500), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("x".repeat(2000)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    assert!(c.build_request(&[]).unwrap().maintenance_needed);
    assert!(c
        .compact(
            CompactionRequest::new(CompactionTrigger::Threshold),
            &CancellationToken::new()
        )
        .await
        .is_err());
    assert!(!c.build_request(&[]).unwrap().maintenance_needed);
    let next = c
        .compact(
            CompactionRequest::new(CompactionTrigger::Threshold),
            &CancellationToken::new(),
        )
        .await
        .unwrap();
    assert_eq!(next.action, CompactAction::Unchanged);
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    // Same model id, new capacity: a previous failed attempt must not suppress maintenance.
    let execution = ContextExecution::new(
        ExecutionBindings {
            model: Some(configured(model.clone(), Some(3500), 500)),
            ..c.execution.bindings().clone()
        },
        c.execution.hooks.clone(),
        c.execution.hook_base.clone(),
    )
    .unwrap();
    assert!(
        c.inner
            .build_request(RequestInput::default(), &execution)
            .unwrap()
            .maintenance_needed
    );
    assert!(c
        .inner
        .compact(
            CompactionRequest::new(CompactionTrigger::Threshold),
            &execution,
            &CancellationToken::new(),
        )
        .await
        .is_err());
    assert_eq!(model.requests.lock().unwrap().len(), 2);
}

struct UncertainStore {
    inner: Arc<SqliteStore>,
    committed: Arc<tokio::sync::Notify>,
    reads: Arc<std::sync::atomic::AtomicUsize>,
}
impl SessionManager for UncertainStore {
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
    fn save_context<'a>(
        &'a self,
        id: &'a SessionId,
        c: ContextChange,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.inner.save_context(id, c).await?;
            self.committed.notify_one();
            std::future::pending().await
        })
    }
    fn create_session<'a>(
        &'a self,
        system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.inner.create_session(system)
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
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        self.inner.fork_session(id)
    }
}
#[tokio::test]
async fn cancelled_uncertain_commit_blocks_reads_and_next_append_recovers_first() {
    let (_dir, store, initial) = setup(policy(), Summarizer::new(), None).await;
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
    let c = MemoryContext::new(
        id,
        configured(Summarizer::new(), Some(12_000), 500),
        services,
    );
    c.restore().await.unwrap();
    let cancel = CancellationToken::new();
    let ctx = c.clone();
    let token = cancel.clone();
    let work = tokio::spawn(async move { ctx.compact(manual(), &token).await });
    committed.notified().await;
    cancel.cancel();
    assert!(work.await.unwrap().is_err());
    assert!(c.build_request(&[]).is_err());
    append(&c, vec![ChatMessage::user("after cancellation")]).await;
    assert!(c.records()[0].summary);
    assert_eq!(c.records().len(), 3);
    assert!(c.build_request(&[]).is_ok());
}

#[tokio::test]
async fn latest_user_protection_uses_persisted_origin_not_text_prefixes() {
    let (_, _, c) = setup(policy(), Summarizer::new(), None).await;
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
    let (_dir, store, initial) = setup(policy(), Summarizer::new(), None).await;
    let reads = Arc::new(AtomicUsize::new(0));
    let id = initial.session_id().clone();
    let mut services = ContextServices::new(&id);
    services.store = Some(Arc::new(UncertainStore {
        inner: store.clone(),
        committed: Default::default(),
        reads: reads.clone(),
    }));
    let c = MemoryContext::new(
        id,
        configured(Summarizer::new(), Some(12_000), 500),
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
    let (_dir, _store, c) = setup(policy(), Arc::new(MediaSummary(model.clone())), None).await;
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
    assert_eq!(result.action, CompactAction::Summarized);
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
/// `MemoryContext::build_request` reject every request whose history
/// contained a Binary part ("media budgeting/capability is not configured").
#[tokio::test]
async fn binary_attachment_builds_request_with_genai_model() {
    use base64::Engine as _;
    let model = yourai_harness::GenaiModel::new(genai::Client::builder().build(), "test-model");
    let (_dir, _store, c) = setup(policy(), Arc::new(model), None).await;
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
    impl HookRuntime for SlowPreCompact {
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
        let context = MemoryContext::new(
            id,
            configured(Summarizer::new(), Some(12_000), 500),
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
async fn checkpoint_bounds_tool_logs_and_anchors_the_active_request() {
    let model = Summarizer::new();
    let (_, _, c) = setup(policy(), model.clone(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("preserve the public API"),
            vec![call("huge")].into(),
            tool(
                "huge",
                json!({"ok": true, "output_paths": ["/tmp/full-output.txt"],
            "output": "log".repeat(100_000)})
                .to_string(),
            ),
            ChatMessage::assistant("next step"),
        ],
    )
    .await;
    let r = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert!(r.verified);
    assert_eq!(r.model_calls, 1);
    assert_eq!(r.summarized_messages, 2);
    assert_eq!(r.retained_messages, 2);
    let requests = model.requests.lock().unwrap();
    let text = serde_json::to_string(&requests[0].request).unwrap();
    assert!(text.len() < 6000);
    assert!(text.contains("preserve the public API"));
    assert!(text.contains("/tmp/full-output.txt"));
    assert!(text.contains("Constraints and preferences"));
    assert_eq!(
        c.records()[1].message.content.first_text(),
        Some("preserve the public API")
    );
}

#[tokio::test]
async fn post_compact_budget_is_verified_and_reported_through_shared_events() {
    let hooks = Arc::new(support::Hooks::new(|inv, result| {
        if inv.event.kind() == HookEventKind::PostCompact {
            if let HookPointOutcome::Generic(out) = &mut result.outcome {
                out.additional_contexts.push("z".repeat(90_000));
            }
        }
    }));
    let (_, _, c) = setup(policy(), Summarizer::new(), Some(hooks)).await;
    append(
        &c,
        vec![
            ChatMessage::user("old task ".repeat(500)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let r = yourai_harness::context::compact_with_events(
        c.inner.as_ref(),
        manual(),
        &c.execution,
        &CancellationToken::new(),
        None,
        &tx,
    )
    .await
    .unwrap();
    let actual = c.build_request(&[]).unwrap();
    assert_eq!(r.tokens_after, actual.estimated_tokens);
    assert_eq!(r.input_budget, actual.input_budget);
    assert!(r.verified && r.stop_reason.is_some());
    assert!(!actual.fits());
    let mut phases = vec![];
    let mut finished = 0;
    while let Ok(out) = rx.try_recv() {
        match out {
            Out::Compaction {
                event: CompactionEvent::Progress { phase, .. },
            } => phases.push(phase),
            Out::Compaction {
                event: CompactionEvent::Finished { result, .. },
            } => {
                assert_eq!(result.tokens_after, actual.estimated_tokens);
                assert!(result.stop_reason.is_some());
                finished += 1;
            }
            _ => panic!("unexpected event"),
        }
    }
    assert_eq!(
        phases,
        [
            CompactionPhase::Preparing,
            CompactionPhase::Summarizing,
            CompactionPhase::Rebuilding
        ]
    );
    assert_eq!(finished, 1);
}

#[tokio::test]
async fn cancellation_after_commit_returns_a_verified_stopped_checkpoint() {
    struct CancelAfterCommit(CancellationToken);
    impl HookRuntime for CancelAfterCommit {
        fn dispatch<'a>(
            &'a self,
            inv: &'a HookInvocation,
        ) -> BoxFuture<'a, Result<HookDispatchResult, YourAiError>> {
            Box::pin(async move {
                if inv.event.kind() == HookEventKind::PostCompact {
                    self.0.cancel();
                    std::future::pending::<()>().await;
                }
                Ok(HookDispatchResult::empty(inv.event.kind()))
            })
        }
    }
    let cancel = CancellationToken::new();
    let (_, _, c) = setup(
        policy(),
        Summarizer::new(),
        Some(Arc::new(CancelAfterCommit(cancel.clone()))),
    )
    .await;
    append(
        &c,
        vec![
            ChatMessage::user("old task ".repeat(500)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    let r = c.compact(manual(), &cancel).await.unwrap();
    assert_eq!(r.action, CompactAction::Summarized);
    assert!(r.verified && r.stop_reason.as_ref().unwrap().contains("committed"));
    assert!(c.records()[0].summary);
    assert_eq!(
        r.tokens_after,
        c.build_request(&[]).unwrap().estimated_tokens
    );
}

#[tokio::test]
async fn summary_savings_do_not_compare_provider_usage_to_a_different_local_estimate() {
    let (_, _, c) = setup(policy(), Summarizer::new(), None).await;
    append(
        &c,
        vec![
            ChatMessage::user("old task".repeat(500)),
            ChatMessage::user("latest"),
        ],
    )
    .await;
    let mut response = StoredMessage::new(ChatMessage::assistant("continue"));
    response.model_response = true;
    response.request_observation = Some(RequestObservation {
        model: "summary".into(),
        request: c.build_request(&[]).unwrap().request,
        input_tokens: 1,
    });
    c.append(vec![response]).await.unwrap();
    let r = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.action, CompactAction::Summarized);
    assert!(r.verified);
    assert_eq!(
        r.tokens_after,
        c.build_request(&[]).unwrap().estimated_tokens
    );
}

#[tokio::test]
async fn recent_turns_are_preserved_as_complete_batches_within_budget() {
    let model = Summarizer::new();
    let mut p = policy();
    p.keep_recent_tokens = 8000;
    p.keep_recent_turns = 2;
    let (_, _, c) = setup(p, model, None).await;
    append(
        &c,
        vec![
            ChatMessage::user("obsolete discussion".repeat(300)),
            ChatMessage::assistant("done"),
            ChatMessage::user("previous request"),
            vec![call("recent")].into(),
            tool("recent", "keep output".into()),
            ChatMessage::user("current request"),
            ChatMessage::assistant("working"),
        ],
    )
    .await;
    let r = c
        .compact(manual(), &CancellationToken::new())
        .await
        .unwrap();
    assert_eq!(r.summarized_messages, 2);
    assert_eq!(r.retained_messages, 5);
    assert!(c
        .records()
        .iter()
        .any(|r| r.message.content.first_text() == Some("previous request")));
    assert!(c.records().iter().any(|r| r
        .message
        .content
        .tool_responses()
        .iter()
        .any(|t| t.call_id == "recent")));
}

#[tokio::test]
async fn automatic_compaction_uses_shared_events_then_resumes_the_same_turn() {
    struct Model {
        summary: Arc<Summarizer>,
        answer: support::Model,
    }
    impl ModelProvider for Model {
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
            request: ModelRequest,
        ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
            self.answer.stream_events(request)
        }
    }
    let model = Arc::new(Model {
        summary: Summarizer::new(),
        answer: support::Model::new(vec![support::answer("continued")]),
    });
    let mut p = policy();
    p.advance_tokens = 2500;
    let (_, _, c) = setup_model(p, configured(model.clone(), Some(4000), 500), None).await;
    append(&c, vec![ChatMessage::user("old task".repeat(500))]).await;
    let agent = Agent::builder()
        .model(configured(model, Some(4000), 500))
        .context_manager(c.clone())
        .agent_loop(Arc::new(
            yourai_harness::default_loop::DefaultLoop::default(),
        ))
        .build();
    let (events, result) =
        support::collect(agent.start(In::user_text("continue the task")).unwrap()).await;
    assert_eq!(result.unwrap().text, "continued");
    let completed = events
        .iter()
        .filter(|e| {
            matches!(e, Out::Compaction {
        event: CompactionEvent::Finished { trigger: CompactionTrigger::Threshold, result }
    } if result.action == CompactAction::Summarized && result.verified)
        })
        .count();
    assert_eq!(completed, 1);
    assert_eq!(
        c.records()
            .iter()
            .filter(|r| r.message.content.first_text() == Some("continue the task"))
            .count(),
        1
    );
}
