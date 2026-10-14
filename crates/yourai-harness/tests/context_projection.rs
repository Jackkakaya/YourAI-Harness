#[path = "support/loop.rs"]
mod support;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
use yourai_core::prelude::*;
use yourai_harness::{tools::ToolOutputStore, DefaultContext, SqliteStore};

fn policy() -> ContextPolicy {
    {
        let mut settings: yourai_core::compactor::ContextPolicy = Default::default();
        settings.set_safety_margin(100);
        settings.set_advance_tokens(Some(200));
        settings.set_keep_recent_tokens(0);
        settings.set_summary_min_savings(1);
        settings
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
        let mut result = StoredMessage::new(output(&id, size));
        // A legacy prune marker must no longer hide the original body.
        if n == 0 {
            result.tool_output_pruned_at = Some(1);
        }
        c.append(vec![
            StoredMessage::new(vec![support::call(&id, "shell")].into()),
            result,
        ])
        .await
        .unwrap();
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
async fn old_outputs_stay_unchanged_across_requests_restore_and_fork() {
    let (db, c) = setup(policy()).await;
    seed_old(&c, 4, 20_000).await;
    let records = c.records();
    let expected = c.project(&records, &[]).unwrap();
    for n in 0..4 {
        assert_eq!(
            response(&expected, &format!("old-{n}")),
            json!({"ok":true,"output":format!("old-{n}").repeat(20_000)})
        );
    }
    append(&c, vec![ChatMessage::user("another request")]).await;
    let model = support::Model::new(vec![]).with_budget(1_000_000, 500);
    for _ in 0..2 {
        let built = c.build_request(&[], &model, &[]).unwrap();
        assert!(built.fits());
        assert!(!built.maintenance_needed);
        for n in 0..4 {
            assert_eq!(
                response(&built.request, &format!("old-{n}")),
                response(&expected, &format!("old-{n}"))
            );
        }
    }
    c.restore().await.unwrap();
    let child = db.fork_session(c.session_id()).await.unwrap();
    let fork = DefaultContext::new(child, String::new(), Some(db), policy());
    fork.restore().await.unwrap();
    for context in [&c, &fork] {
        let request = context.project(&context.records(), &[]).unwrap();
        for n in 0..4 {
            assert_eq!(
                response(&request, &format!("old-{n}")),
                response(&expected, &format!("old-{n}"))
            );
        }
    }
}

struct OutputTool(Value);
impl ToolProvider for OutputTool {
    fn security_context(&self, input: &Value, _: Option<&Path>) -> SecurityContext {
        SecurityContext {
            action: "mcp__test__output".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("mcp__test__output").with_schema(
            json!({"type":"object","properties":{"value":{"type":"integer"}},"required":["value"]}),
        )
    }
    fn run<'a>(
        &'a self,
        _: ToolContext<'a>,
        _input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move { Ok(self.0.clone()) })
    }
}

#[tokio::test]
async fn output_is_bounded_before_after_hook_and_hook_changes_reach_the_model_unchanged() {
    for size in [200, 90_000] {
        let (db, c) = setup(policy()).await;
        let outputs = ToolOutputStore::new(db.clone(), c.session_id().clone());
        let original =
            json!({"ok":true,"exit_code":0,"content":format!("HEAD{}TAIL", "x".repeat(size))});
        let seen = Arc::new(std::sync::Mutex::new(None));
        let hook_seen = seen.clone();
        let hooks = Arc::new(support::Hooks::new(move |invocation, result| {
            if let HookEvent::PostToolUse { tool_response, .. } = &invocation.event {
                *hook_seen.lock().unwrap() = Some(tool_response.clone());
                if let HookPointOutcome::PostToolUse(outcome) = &mut result.outcome {
                    let mut final_output = tool_response.clone();
                    final_output["after_hook"] = json!(true);
                    outcome.updated_mcp_tool_output = Some(final_output);
                }
            }
        }));
        let tools = Arc::new(support::Registry::default());
        tools.register(Arc::new(OutputTool(original.clone())));
        let model = Arc::new(
            support::Model::new(vec![
                support::calls(&["mcp__test__output"]),
                support::answer("done"),
            ])
            .with_budget(1_000_000, 500),
        );
        let agent = Agent::builder()
            .model(model.clone())
            .context_manager(c.clone())
            .tools(tools)
            .hooks(hooks)
            .agent_loop(Arc::new(yourai_harness::default_loop::DefaultLoop))
            .tool_output(outputs.clone())
            .build();
        assert_eq!(
            agent.run(In::user_text("run tool")).await.unwrap().text,
            "done"
        );
        let stored = c
            .records()
            .into_iter()
            .flat_map(|m| {
                m.message
                    .content
                    .tool_responses()
                    .into_iter()
                    .cloned()
                    .collect::<Vec<_>>()
            })
            .find(|r| r.call_id == "c0")
            .unwrap()
            .content;
        let preview: Value = serde_json::from_str(&stored).unwrap();
        assert_eq!(preview["after_hook"], true);
        let mut hook_input = preview.clone();
        hook_input.as_object_mut().unwrap().remove("after_hook");
        assert_eq!(*seen.lock().unwrap(), Some(hook_input.clone()));
        if size > 50 * 1024 {
            assert_eq!(preview["truncated"], true);
            assert_eq!(preview["status"]["exit_code"], 0);
            assert!(stored.len() > 16_000);
            assert!(preview["content"].as_str().unwrap().contains("HEAD"));
            assert!(preview["content"].as_str().unwrap().contains("TAIL"));
            let reference = preview["output_refs"][0].as_str().unwrap();
            let raw = outputs.read(reference, 0, 1024 * 1024).await.unwrap();
            assert_eq!(serde_json::from_slice::<Value>(&raw).unwrap(), original);
        } else {
            assert_eq!(hook_input, original);
        }
        {
            let requests = model.requests.lock().unwrap();
            assert_eq!(requests.len(), 2);
            assert_eq!(
                requests[1]
                    .request
                    .messages
                    .iter()
                    .flat_map(|m| m.content.tool_responses())
                    .find(|r| r.call_id == "c0")
                    .unwrap()
                    .content,
                stored
            );
        }
        append(
            &c,
            vec![ChatMessage::user("later"), ChatMessage::user("even later")],
        )
        .await;
        c.restore().await.unwrap();
        assert_eq!(
            response(
                &c.build_request(&[], model.as_ref(), &[]).unwrap().request,
                "c0"
            ),
            preview
        );
    }
}

#[tokio::test]
async fn old_outputs_over_budget_trigger_summary_instead_of_age_pruning() {
    let (_, c) = setup(policy()).await;
    seed_old(&c, 4, 600).await;
    let model = Arc::new(support::Model::new(vec![support::answer("done")]).with_budget(4000, 500));
    assert!(!c.build_request(&[], model.as_ref(), &[]).unwrap().fits());
    let agent = Agent::builder()
        .model(model.clone())
        .context_manager(c.clone())
        .agent_loop(Arc::new(yourai_harness::default_loop::DefaultLoop))
        .build();
    assert_eq!(
        agent.run(In::user_text("continue")).await.unwrap().text,
        "done"
    );
    assert!(c.records().iter().any(|r| r.summary));
    assert_eq!(model.requests.lock().unwrap().len(), 2); // Summary, then the normal answer.
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
