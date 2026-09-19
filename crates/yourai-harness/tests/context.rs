#[path = "support/context.rs"]
mod context_fixture;
use context_fixture::{ContextServices, MemoryContext};
#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::sync::{Arc, Mutex};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
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
        context_window: Some(12_000),
        output_reserve: 500,
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
async fn projection_is_bounded_valid_json_and_original_is_pageable() {
    let mut p = policy();
    p.tool_output_chars = 512;
    let (_dir, store, c) = setup(p, Summarizer::new(), None).await;
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
    let reader = yourai_harness::tools::result::ReadToolResult {
        session: c.session_id().clone(),
        store,
        max_chars: 512,
    };
    let cancel = CancellationToken::new();
    let mut offset = 0;
    let mut reconstructed = String::new();
    loop {
        let page = reader
            .execute(
                ToolContext {
                    call_id: "read".into(),
                    emit: &DiscardSink,
                    cancel: &cancel,
                    security: None,
                    sandbox: None,
                    interaction: None,
                },
                json!({"call_id":"a","offset":offset,"limit":10000}),
            )
            .await
            .unwrap();
        assert!(page.to_string().chars().count() <= 512);
        reconstructed.push_str(page["content"].as_str().unwrap());
        match page["next"].as_u64() {
            Some(next) => {
                assert!(next > offset);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(reconstructed, original);
}
#[tokio::test]
async fn prune_only_avoids_model_hooks_and_survives_restore_and_fork() {
    let mut p = policy();
    p.context_window = Some(4000);
    p.advance_tokens = 2200;
    p.prune_enabled = true;
    p.prune_growth = 0;
    p.prune_min_savings = 50;
    let model = Summarizer::new();
    let hooks = Arc::new(support::Hooks::new(|_, _| {}));
    let (_dir, store, c) = setup(p.clone(), model.clone(), Some(hooks.clone())).await;
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
    let fork = MemoryContext::new(child, model, services);
    fork.restore().await.unwrap();
    assert_eq!(
        expected,
        serde_json::to_value(fork.build_request(&[]).unwrap().request).unwrap()
    );
}
#[tokio::test]
async fn failed_combined_commit_keeps_pruning_and_summary_unapplied_but_accounts_usage() {
    let mut p = policy();
    p.context_window = Some(4000);
    p.advance_tokens = 3300;
    p.prune_enabled = true;
    p.prune_growth = 0;
    p.prune_min_savings = 50;
    let model = Summarizer::new();
    let (dir, store, c) = setup(p, model.clone(), None).await;
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
    assert_eq!(
        req.messages
            .iter()
            .flat_map(|m| m.content.tool_calls())
            .count(),
        2
    );
    assert_eq!(
        req.messages
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .count(),
        2
    );
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
    p.context_window = Some(2200);
    p.output_reserve = 200;
    p.safety_margin = 100;
    let model = Summarizer::new();
    let (_, store, c) = setup(p, model.clone(), None).await;
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
    let mut p = policy();
    p.context_window = None;
    let model = Summarizer::new();
    let (_, _, c) = setup(p, model.clone(), None).await;
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
    p.context_window = Some(4000);
    p.advance_tokens = 3000;
    let model = Arc::new(Summarizer {
        requests: Mutex::new(vec![]),
        empty: true,
    });
    let (_, _, c) = setup(p, model.clone(), None).await;
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
    let c = MemoryContext::new(id, Summarizer::new(), services);
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
    let c = MemoryContext::new(id, Summarizer::new(), services);
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
