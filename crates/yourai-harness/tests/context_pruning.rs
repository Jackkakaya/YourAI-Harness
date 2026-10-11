#[path = "support/loop.rs"]
mod support;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::{
    tools::{Read, ToolOutputStore},
    DefaultContext, SqliteStore,
};

fn policy() -> ContextPolicy {
    ContextPolicy {
        safety_margin: 100,
        advance_tokens: 200,
        tool_output_chars: 4096,
        tool_prune_keep_tokens: 0,
        tool_prune_min_savings: 100,
        ..Default::default()
    }
}
async fn setup(p: ContextPolicy) -> (Arc<SqliteStore>, Arc<DefaultContext>) {
    let db = Arc::new(SqliteStore::open(Path::new(":memory:")).unwrap());
    let id = db
        .create_session(SessionId::new(), "system")
        .await
        .unwrap()
        .id;
    let context = DefaultContext::new(id, "system".into(), Some(db.clone()), p);
    context.restore().await.unwrap();
    (db, context)
}
async fn append(c: &DefaultContext, messages: Vec<ChatMessage>) {
    c.append(messages.into_iter().map(StoredMessage::new).collect())
        .await
        .unwrap();
}
fn output(id: &str, size: usize) -> ChatMessage {
    ToolResponse::new(id, json!({"ok":true,"output":id.repeat(size)}).to_string()).into()
}
fn response(request: &ChatRequest, id: &str) -> Value {
    let response = request
        .messages
        .iter()
        .flat_map(|m| m.content.tool_responses())
        .find(|r| r.call_id == id)
        .unwrap();
    serde_json::from_str(&response.content).unwrap()
}
async fn seed_old(c: &DefaultContext, count: usize, size: usize) {
    append(c, vec![ChatMessage::user("old task")]).await;
    for n in 0..count {
        let id = format!("old-{n}");
        append(
            c,
            vec![vec![support::call(&id, "shell")].into(), output(&id, size)],
        )
        .await;
    }
    append(
        c,
        vec![
            ChatMessage::user("previous request"),
            ChatMessage::user("current request"),
        ],
    )
    .await;
}

#[tokio::test]
async fn pruning_preserves_recent_turns_status_archive_and_restore() {
    let (db, c) = setup(policy()).await;
    let blob = ToolOutputStore::new(db.clone(), c.session_id().clone());
    let blob_ref = blob.create();
    blob.append(&blob_ref, 0, b"full shell output")
        .await
        .unwrap();
    let original =
        json!({"ok":false,"error":"command failed","exit_code":7,"termination":"exit","output_complete":false,"output_refs":[blob_ref],"output":"旧输出🌍".repeat(600)})
            .to_string();
    append(
        &c,
        vec![
            ChatMessage::user("old task"),
            vec![support::call("old", "shell")].into(),
            ToolResponse::new("old", original.clone()).into(),
            ChatMessage::user("previous request"),
            vec![support::call("previous", "shell")].into(),
            output("previous", 1000),
            ChatMessage::user("current request"),
            vec![support::call("current", "shell")].into(),
            output("current", 1000),
        ],
    )
    .await;
    let records = c.records();
    let projected = c.project(&records, &[]).unwrap();
    let pruned = response(&projected, "old");
    assert_eq!(pruned["pruned"], true);
    assert_eq!(
        pruned["status"],
        json!({"ok":false,"error":"command failed","exit_code":7,"termination":"exit","output_complete":false})
    );
    assert_eq!(pruned["head"], "");
    assert_eq!(pruned["tail"], "");
    assert_ne!(response(&projected, "previous")["pruned"], true);
    assert_ne!(response(&projected, "current")["pruned"], true);
    assert_eq!(
        projected
            .messages
            .iter()
            .flat_map(|m| m.content.tool_calls())
            .count(),
        3
    );
    assert_eq!(
        projected
            .messages
            .iter()
            .flat_map(|m| m.content.tool_responses())
            .count(),
        3
    );
    let stored = db
        .read_messages(c.session_id(), MessageQuery::default())
        .await
        .unwrap();
    assert_eq!(
        stored.messages[2].message.content.tool_responses()[0].content,
        original
    );
    assert!(stored
        .messages
        .iter()
        .all(|m| m.tool_output_pruned_at.is_none()));
    assert_eq!(pruned["output_refs"][0], blob_ref);
    let reference = pruned["output_refs"][1].as_str().unwrap();
    let outputs = ToolOutputStore::new(db.clone(), c.session_id().clone());
    let read = Read::with_output(Path::new("/").to_owned(), Some(outputs));
    let cancel = CancellationToken::new();
    let result = read
        .run(
            ToolContext {
                providers: None,
                cwd: None,
                call_id: "read-old".into(),
                emit: &DiscardSink,
                cancel: &cancel,
                security: None,
                sandbox: None,
                interaction: None,
            },
            json!({"path":reference,"byte_offset":0,"byte_limit":50*1024}),
        )
        .await
        .unwrap();
    assert_eq!(result["content"], original);
    c.restore().await.unwrap();
    assert_eq!(
        serde_json::to_value(&projected).unwrap(),
        serde_json::to_value(c.project(&c.records(), &[]).unwrap()).unwrap()
    );
    let fork_id = db.fork_session(c.session_id()).await.unwrap();
    let fork = DefaultContext::new(fork_id, String::new(), Some(db.clone()), policy());
    fork.restore().await.unwrap();
    let projected_fork = fork.project(&fork.records(), &[]).unwrap();
    let reference = response(&projected_fork, "old")["output_refs"][1]
        .as_str()
        .unwrap()
        .to_owned();
    // The fork resolves references against its own copied history.
    let fork_db = fork.records();
    assert_eq!(
        reference,
        format!("tool-output://message/{}", fork_db[2].seq)
    );
    db.delete_session(c.session_id()).await.unwrap();
    let fork_outputs = ToolOutputStore::new(db, fork.session_id().clone());
    assert_eq!(
        fork_outputs.read(&blob_ref, 0, 50 * 1024).await.unwrap(),
        b"full shell output"
    );
    assert_eq!(
        fork_outputs.read(&reference, 0, 50 * 1024).await.unwrap(),
        original.as_bytes()
    );
}

#[tokio::test]
async fn older_output_budget_protects_whole_results() {
    let mut p = policy();
    p.tool_prune_keep_tokens = 1000;
    let (_, c) = setup(p).await;
    // Each projected result costs about 800 tokens. The crossing result is
    // retained whole, so the newest two old results exceed the protection floor.
    seed_old(&c, 4, 400).await;
    let request = c.project(&c.records(), &[]).unwrap();
    for n in 0..4 {
        assert_eq!(
            response(&request, &format!("old-{n}"))["pruned"] == true,
            n < 2
        );
    }
}

#[tokio::test]
async fn minimum_savings_uses_bounded_request_and_zero_disables_pruning() {
    let mut p = policy();
    p.tool_output_chars = 512;
    p.tool_prune_min_savings = 1000;
    let (db, c) = setup(p).await;
    seed_old(&c, 1, 100_000).await;
    let request = c.project(&c.records(), &[]).unwrap();
    // The archive is huge, but its already bounded preview cannot save 1000 tokens.
    assert_ne!(response(&request, "old-0")["pruned"], true);
    let mut disabled = policy();
    disabled.tool_prune_min_savings = 0;
    let disabled = DefaultContext::new(c.session_id().clone(), String::new(), Some(db), disabled);
    disabled.restore().await.unwrap();
    let request = disabled.project(&disabled.records(), &[]).unwrap();
    assert_ne!(response(&request, "old-0")["pruned"], true);
}

#[tokio::test]
async fn runtime_notes_do_not_age_results_and_unresolved_batches_are_preserved() {
    let (_, c) = setup(policy()).await;
    append(
        &c,
        vec![
            ChatMessage::user("old task"),
            vec![support::call("a", "shell")].into(),
            output("a", 1000),
            ChatMessage::user("current task"),
        ],
    )
    .await;
    c.append(vec![
        StoredMessage::runtime_context("hook note"),
        StoredMessage::runtime_context("another hook note"),
    ])
    .await
    .unwrap();
    assert_ne!(
        response(&c.project(&c.records(), &[]).unwrap(), "a")["pruned"],
        true
    );
    let (_, pending) = setup(policy()).await;
    append(
        &pending,
        vec![
            ChatMessage::user("old task"),
            vec![support::call("a", "shell"), support::call("b", "shell")].into(),
            output("a", 1000),
            ChatMessage::user("previous"),
            ChatMessage::user("current"),
        ],
    )
    .await;
    assert_ne!(
        response(&pending.project(&pending.records(), &[]).unwrap(), "a")["pruned"],
        true
    );
}

#[tokio::test]
async fn pruning_reduces_request_before_automatic_summary_without_model_call() {
    let p = policy();
    let (db, c) = setup(p.clone()).await;
    seed_old(&c, 4, 600).await;
    let model = Arc::new(support::Model::new(vec![support::answer("done")]).with_budget(4000, 500));
    let mut disabled = p;
    disabled.tool_prune_min_savings = 0;
    let baseline = DefaultContext::new(c.session_id().clone(), String::new(), Some(db), disabled);
    baseline.restore().await.unwrap();
    assert!(!baseline
        .build_request(&[], model.as_ref(), &[])
        .unwrap()
        .fits());
    assert!(
        !c.build_request(&[], model.as_ref(), &[])
            .unwrap()
            .maintenance_needed
    );
    let hooks = Arc::new(support::Hooks::new(|_, _| {}));
    let agent = Agent::builder()
        .model(model.clone())
        .context_manager(c.clone())
        .hooks(hooks.clone())
        .agent_loop(Arc::new(yourai_harness::default_loop::DefaultLoop::new(
            Default::default(),
        )))
        .build();
    let result = agent.run(In::user_text("continue")).await.unwrap();
    assert_eq!(result.text, "done");
    let requests = model.requests.lock().unwrap();
    assert_eq!(requests.len(), 1); // Only the normal answer stream; no paid summary.
    assert_eq!(response(&requests[0].request, "old-0")["pruned"], true);
    assert!(c.records().iter().all(|r| !r.summary));
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PreCompact));
    assert!(!hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::PostCompact));
}

#[tokio::test]
async fn history_output_reads_are_bounded_session_local_and_survive_summary() {
    let (db, c) = setup(policy()).await;
    seed_old(&c, 1, 1000).await;
    let row = c.records()[2].clone();
    let original = row.message.content.tool_responses()[0].content.clone();
    let reference = format!("tool-output://message/{}", row.seq);
    let outputs = ToolOutputStore::new(db.clone(), c.session_id().clone());
    assert_eq!(
        outputs.read(&reference, 5, 17).await.unwrap(),
        original.as_bytes()[5..22]
    );
    assert!(outputs.read(&reference, 0, 1024 * 1024 + 1).await.is_err());
    assert!(outputs
        .read("tool-output://message/0", 0, 10)
        .await
        .is_err());
    assert!(outputs
        .read("tool-output://message/1", 0, 10)
        .await
        .is_err());
    let other = db.create_session(SessionId::new(), "").await.unwrap().id;
    assert!(ToolOutputStore::new(db.clone(), other)
        .read(&reference, 0, 10)
        .await
        .is_err());
    let mut summary = StoredMessage::new(ChatMessage::user("summary"));
    summary.summary = true;
    db.save_summary(
        c.session_id(),
        vec![row.id],
        summary,
        Arc::new(std::sync::atomic::AtomicU8::new(0)),
    )
    .await
    .unwrap();
    assert_eq!(
        outputs.read(&reference, 0, 50 * 1024).await.unwrap(),
        original.as_bytes()
    );
}

#[tokio::test]
async fn pruning_keeps_tool_media_and_never_prunes_ephemeral_history() {
    let (_, c) = setup(policy()).await;
    let media = ContentPart::Binary(Binary::from_url(
        "image/png",
        "https://example.invalid/old.png",
        None,
    ));
    let tool_part = ContentPart::ToolResponse(ToolResponse::new(
        "old",
        json!({"ok":true,"output":"x".repeat(3000)}).to_string(),
    ));
    let messages = vec![
        ChatMessage::user("old task"),
        vec![support::call("old", "shell")].into(),
        ChatMessage {
            role: ChatRole::Tool,
            content: MessageContent::from_parts(vec![media, tool_part]),
            ..ToolResponse::new("unused", "").into()
        },
        ChatMessage::user("previous"),
        ChatMessage::user("current"),
    ];
    append(&c, messages.clone()).await;
    let projected = c.project(&c.records(), &[]).unwrap();
    assert_eq!(response(&projected, "old")["pruned"], true);
    assert!(projected
        .messages
        .iter()
        .flat_map(|m| m.content.parts())
        .any(|p| matches!(p, ContentPart::Binary(_))));
    let ephemeral = DefaultContext::new(SessionId::new(), String::new(), None, policy());
    append(&ephemeral, messages).await;
    assert_ne!(
        response(
            &ephemeral.project(&ephemeral.records(), &[]).unwrap(),
            "old"
        )["pruned"],
        true
    );
}
