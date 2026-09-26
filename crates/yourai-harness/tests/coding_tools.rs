#[path = "support/context.rs"]
mod context_fixture;
#[path = "support/loop.rs"]
mod support;
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use support::*;
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::*;
fn invoke(id: &str, name: &str, args: Value) -> ModelEventStream {
    let mut c = call(id, name);
    c.fn_arguments = args;
    events(vec![end("", vec![c])])
}
struct Capture(Mutex<Vec<Out>>);
impl OutSink for Capture {
    fn send(&self, o: Out) -> bool {
        self.0.lock().unwrap().push(o);
        true
    }
}
#[tokio::test]
async fn coding_loop_modifies_tests_and_persists_results() {
    let d = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    let model = Arc::new(Model::new(vec![
        invoke(
            "write-1",
            "write",
            json!({"path":"answer.txt","content":"wrong\n"}),
        ),
        invoke("read-1", "read", json!({"path":"answer.txt"})),
        invoke(
            "test-1",
            "shell",
            json!({"command":"test \"$(cat answer.txt)\" = correct || { echo 'expected correct' >&2; exit 1; }"}),
        ),
        invoke(
            "edit-1",
            "edit",
            json!({"path":"answer.txt","old_text":"wrong","new_text":"correct"}),
        ),
        invoke(
            "test-2",
            "shell",
            json!({"command":"test \"$(cat answer.txt)\" = correct && echo passed"}),
        ),
        answer("fixed and tested"),
    ]));
    let mut cfg = HarnessConfig::new(root.clone(), d.path().into());
    cfg.trusted_shell = true;
    let h = Harness::open(cfg, model.clone()).await.unwrap();
    let capture = Capture(Mutex::new(vec![]));
    h.host
        .submit(In::user_text("fix the answer and test"))
        .unwrap();
    h.host
        .run_next(TurnLimits::default(), &capture, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(d.path().join("answer.txt")).unwrap(),
        "correct\n"
    );
    assert!(!capture
        .0
        .lock()
        .unwrap()
        .iter()
        .any(|o| matches!(o, Out::Ask { .. })));
    assert!(capture.0.lock().unwrap().iter().any(|o|matches!(o,Out::ToolDone{name,output,is_error:false,..} if name=="shell" && output["exit_code"]==1 && output["stderr"].as_str().unwrap().contains("expected correct"))));
    let offered = model.requests.lock().unwrap()[0]
        .request
        .tools
        .clone()
        .unwrap();
    for name in [
        "read",
        "write",
        "edit",
        "shell",
        "read_tool_result",
        "webfetch",
        "websearch",
    ] {
        assert!(offered.iter().any(|t| t.name.as_str() == name));
    }
    let id = h.host.context().id;
    h.close().await.unwrap();
    drop(h);
    let catalog = SessionCatalog::new(&root).unwrap();
    let rows = catalog
        .read_messages(
            &id,
            MessageQuery {
                after: 0,
                active_only: false,
                limit: 100,
            },
        )
        .await
        .unwrap();
    assert_eq!(
        rows.messages
            .iter()
            .filter(|r| r.message.role == ChatRole::Tool)
            .count(),
        5
    );
    let model = Arc::new(Model::new(vec![
        invoke("fetch", "read_tool_result", json!({"call_id":"test-1"})),
        answer("restored"),
    ]));
    let mut cfg = HarnessConfig::new(root, d.path().into());
    cfg.resume = Some(id);
    let h = Harness::open(cfg, model).await.unwrap();
    // read_tool_result is an existing separate tool; explicitly grant its permission.
    HookRegistry::register(
        h.hooks.as_ref(),
        NativeHookRegistration {
            id: "allow-result".into(),
            event: HookEventKind::PermissionRequest,
            matcher: None,
            handler: Arc::new(Approve),
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        },
    )
    .await
    .unwrap();
    h.host
        .submit(In::user_text("read the earlier test output"))
        .unwrap();
    let capture = Capture(Mutex::new(vec![]));
    h.host
        .run_next(TurnLimits::default(), &capture, &CancellationToken::new())
        .await
        .unwrap()
        .unwrap()
        .result
        .unwrap();
    assert!(capture.0.lock().unwrap().iter().any(|o|matches!(o,Out::ToolDone{name,output,..} if name=="read_tool_result" && output["content"].as_str().unwrap().contains("expected correct"))));
    h.close().await.unwrap();
}
struct Approve;
impl HookHandler for Approve {
    fn execute<'a>(
        &'a self,
        _: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        Box::pin(async {
            Ok(HookOutput::Parsed(
                json!({"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow"}}}),
            ))
        })
    }
}
#[tokio::test]
async fn local_policy_requires_shell_and_external_approval_and_honors_hard_deny() {
    let d = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    let policy = PolicySecurity::open_for_workspace(
        root.join("p.json"),
        vec!["edit".into()],
        root.clone(),
        false,
    )
    .unwrap();
    let tools = yourai_harness::tools::coding_tools(&root).unwrap();
    for (name, input, expected) in [
        ("read", json!({"path":"a"}), ApprovalDecision::Allow),
        ("write", json!({"path":"src/a"}), ApprovalDecision::Allow),
        ("edit", json!({"path":"a"}), ApprovalDecision::Deny),
        (
            "read",
            json!({"path":outside.path().join("a")}),
            ApprovalDecision::Ask,
        ),
        ("shell", json!({"command":"pwd"}), ApprovalDecision::Ask),
    ] {
        let tool = tools.iter().find(|t| t.name() == name).unwrap();
        assert_eq!(
            policy
                .check_tool_call(&tool.security_context(&input))
                .await
                .unwrap(),
            expected
        );
    }
}
#[tokio::test]
async fn denied_shell_does_not_run_and_next_model_request_gets_failure() {
    let d = TempDir::new().unwrap();
    let model = Arc::new(Model::new(vec![
        invoke("denied", "shell", json!({"command":"echo bad > bad.txt"})),
        answer("denied"),
    ]));
    let h = Harness::open(
        HarnessConfig::new(d.path().join("sessions"), d.path().into()),
        model.clone(),
    )
    .await
    .unwrap();
    h.host.submit(In::user_text("test denial")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = h.host.clone();
    let task = tokio::spawn(async move {
        host.run_next(TurnLimits::default(), &tx, &CancellationToken::new())
            .await
    });
    while let Some(event) = rx.recv().await {
        if let Out::Ask { id, .. } = event {
            h.host
                .submit(In::Reply {
                    id,
                    payload: json!({"behavior":"deny"}),
                })
                .unwrap();
        }
    }
    task.await.unwrap().unwrap().unwrap().result.unwrap();
    assert!(!d.path().join("bad.txt").exists());
    assert!(model.requests.lock().unwrap()[1]
        .request
        .messages
        .iter()
        .any(|m| m.role == ChatRole::Tool));
    h.close().await.unwrap();
}
#[tokio::test]
async fn child_restore_rebinds_builtin_working_directory() {
    let d = TempDir::new().unwrap();
    let child = TempDir::new().unwrap();
    let root = d.path().join("sessions");
    let h = Harness::open(
        HarnessConfig::new(root.clone(), d.path().into()),
        Arc::new(Model::new(vec![])),
    )
    .await
    .unwrap();
    let catalog = SessionCatalog::new(&root).unwrap();
    let id = catalog.create_session("child").await.unwrap().id;
    let model = Arc::new(Model::new(vec![
        invoke(
            "write-child",
            "write",
            json!({"path":"child.txt","content":"here"}),
        ),
        answer("done"),
    ]));
    let host = SessionHost::restore(
        &root,
        id,
        child.path().into(),
        model,
        None,
        Some(h.tools.clone()),
        ContextPolicy::default(),
        "startup",
    )
    .await
    .unwrap();
    host.submit(In::user_text("create a file")).unwrap();
    host.run_next(
        TurnLimits::default(),
        &DiscardSink,
        &CancellationToken::new(),
    )
    .await
    .unwrap()
    .unwrap()
    .result
    .unwrap();
    assert!(child.path().join("child.txt").exists());
    assert!(!d.path().join("child.txt").exists());
    host.close(Some(std::time::Duration::from_secs(1)))
        .await
        .unwrap();
    h.close().await.unwrap();
}

struct ForcePermissionAsk;
impl HookHandler for ForcePermissionAsk {
    fn execute<'a>(
        &'a self,
        _: &'a HookInvocation,
    ) -> BoxFuture<'a, Result<HookOutput, YourAiError>> {
        Box::pin(async {
            Ok(HookOutput::Parsed(
                json!({"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"ask"}}),
            ))
        })
    }
}
#[tokio::test]
async fn yolo_skips_all_permissions_including_children_but_preserves_questions() {
    let d = TempDir::new().unwrap();
    let outside = TempDir::new().unwrap();
    let path = outside.path().join("outside.txt");
    let model = Arc::new(Model::new(vec![
        invoke(
            "external",
            "write",
            json!({"path":path,"content":"approved by mode"}),
        ),
        invoke("delegate", "subagent", json!({"prompt":"check the shell"})),
        invoke("child-shell", "shell", json!({"command":"printf child-ok"})),
        answer("child done"),
        invoke("question", "ask_user", json!({"value":1})),
        answer("all done"),
    ]));
    let mut cfg = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    cfg.yolo = true;
    cfg.extensions = true;
    let h = Harness::open(cfg, model).await.unwrap();
    h.tools
        .register(Arc::new(Handler::new("ask_user", Mode::Ask)));
    HookRegistry::register(
        h.hooks.as_ref(),
        NativeHookRegistration {
            id: "force-ask".into(),
            event: HookEventKind::PreToolUse,
            matcher: None,
            handler: Arc::new(ForcePermissionAsk),
            timeout: None,
            source: HookSource::Session,
            failure_policy: FailurePolicy::Closed,
            once: false,
        },
    )
    .await
    .unwrap();
    h.host.submit(In::user_text("run everything")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = h.host.clone();
    let task = tokio::spawn(async move {
        host.run_next(TurnLimits::default(), &tx, &CancellationToken::new())
            .await
    });
    let mut questions = 0;
    tokio::time::timeout(std::time::Duration::from_secs(5), async {
        while let Some(event) = rx.recv().await {
            if let Out::Ask { id, payload } = event {
                assert_ne!(payload["kind"], "permission");
                assert!(
                    payload.get("child_id").is_none(),
                    "child permission must also be skipped"
                );
                questions += 1;
                h.host
                    .submit(In::Reply {
                        id,
                        payload: json!("answer"),
                    })
                    .unwrap();
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(questions, 2);
    task.await.unwrap().unwrap().unwrap().result.unwrap();
    assert_eq!(std::fs::read_to_string(path).unwrap(), "approved by mode");
    let id = h.host.context().id;
    h.close().await.unwrap();
    drop(h);
    let mut cfg = HarnessConfig::new(d.path().join("sessions"), d.path().into());
    cfg.resume = Some(id);
    let h = Harness::open(
        cfg,
        Arc::new(Model::new(vec![
            invoke("normal", "shell", json!({"command":"echo should-not-run"})),
            answer("done"),
        ])),
    )
    .await
    .unwrap();
    h.host.submit(In::user_text("normal mode again")).unwrap();
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let host = h.host.clone();
    let task = tokio::spawn(async move {
        host.run_next(TurnLimits::default(), &tx, &CancellationToken::new())
            .await
    });
    let mut permissions = 0;
    while let Some(event) = rx.recv().await {
        if let Out::Ask { id, payload } = event {
            assert_eq!(payload["kind"], "permission");
            permissions += 1;
            h.host
                .submit(In::Reply {
                    id,
                    payload: json!({"behavior":"deny"}),
                })
                .unwrap();
        }
    }
    assert_eq!(permissions, 1);
    task.await.unwrap().unwrap().unwrap().result.unwrap();
    h.close().await.unwrap();
}
