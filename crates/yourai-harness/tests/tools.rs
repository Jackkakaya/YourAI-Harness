use serde_json::{json, Value};
use std::{path::Path, sync::Arc, time::Duration};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{context::DiscardSink, prelude::*};
use yourai_harness::tools::*;

async fn call(tool: &dyn ToolHandler, input: Value) -> Result<Value, YourAiError> {
    let cancel = CancellationToken::new();
    tool.execute(
        ToolContext {
            call_id: "test".into(),
            emit: &DiscardSink,
            cancel: &cancel,
            security: None,
            sandbox: None,
            interaction: None,
        },
        input,
    )
    .await
}
#[tokio::test]
async fn file_roundtrip_paging_exact_edit_and_atomic_rejection() {
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    let write = Write::new(root.clone());
    let read = Read::new(root.clone());
    let edit = Edit::new(root.clone());
    let path = root.join("src/a.rs");
    let r = call(
        &write,
        json!({"path":"src/a.rs","content":"one\ntwo\nthree\n"}),
    )
    .await
    .unwrap();
    assert_eq!(r["created"], true);
    let r = call(&read, json!({"path":"src/a.rs","offset":2,"limit":1}))
        .await
        .unwrap();
    assert_eq!(r["content"], "2|two\n");
    assert_eq!(r["next_offset"], 3);
    assert!(call(
        &edit,
        json!({"path":"src/a.rs","old_text":"absent","new_text":"x"})
    )
    .await
    .is_err());
    assert_eq!(std::fs::read_to_string(&path).unwrap(), "one\ntwo\nthree\n");
    let r = call(
        &edit,
        json!({"path":"src/a.rs","old_text":"two","new_text":"TWO"}),
    )
    .await
    .unwrap();
    assert!(r["diff"].as_str().unwrap().contains("+TWO"));
    call(&write, json!({"path":"src/a.rs","content":"aaa"}))
        .await
        .unwrap();
    // Include overlapping occurrences: aa appears twice in aaa.
    assert!(call(
        &edit,
        json!({"path":"src/a.rs","old_text":"aa","new_text":"b"})
    )
    .await
    .is_err());
    assert_eq!(std::fs::read_to_string(path).unwrap(), "aaa");
    call(&write, json!({"path":"empty","content":""}))
        .await
        .unwrap();
    assert_eq!(
        call(&read, json!({"path":"empty"})).await.unwrap()["content"],
        ""
    );
    assert!(call(&read, json!({"path":"empty","offset":2}))
        .await
        .is_err());
    std::fs::write(root.join("binary"), [0u8, 255]).unwrap();
    assert!(call(&read, json!({"path":"binary"})).await.is_err());
}
#[cfg(unix)]
#[tokio::test]
async fn symlinks_permissions_line_endings_and_cancelled_write() {
    use std::os::unix::fs::{symlink, PermissionsExt};
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    std::fs::write(root.join("a"), "\u{feff}one\r\ntwo\r\n").unwrap();
    std::fs::set_permissions(root.join("a"), std::fs::Permissions::from_mode(0o750)).unwrap();
    let edit = Edit::new(root.clone());
    call(&edit, json!({"path":"a","old_text":"two","new_text":"TWO"}))
        .await
        .unwrap();
    assert_eq!(
        std::fs::read_to_string(root.join("a")).unwrap(),
        "\u{feff}one\r\nTWO\r\n"
    );
    assert_eq!(
        std::fs::metadata(root.join("a"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o750
    );
    symlink(root.join("a"), root.join("link")).unwrap();
    assert!(call(
        &edit,
        json!({"path":"link","old_text":"one","new_text":"bad"})
    )
    .await
    .is_err());
    let write = Write::new(root.clone());
    assert!(call(&write, json!({"path":"link","content":"bad"}))
        .await
        .is_err());
    let cancel = CancellationToken::new();
    cancel.cancel();
    assert!(write
        .execute(
            ToolContext {
                call_id: "cancel".into(),
                emit: &DiscardSink,
                cancel: &cancel,
                security: None,
                sandbox: None,
                interaction: None
            },
            json!({"path":"a","content":"bad"})
        )
        .await
        .is_err());
    assert!(std::fs::read_to_string(root.join("a"))
        .unwrap()
        .contains("TWO"));
    let elsewhere = TempDir::new().unwrap();
    std::fs::create_dir(elsewhere.path().join("child")).unwrap();
    symlink(elsewhere.path().join("child"), root.join("outside")).unwrap();
    assert_eq!(
        resolve_path(&root, Path::new("outside/../new")).unwrap(),
        elsewhere.path().canonicalize().unwrap().join("new")
    );
}
#[cfg(unix)]
#[tokio::test]
async fn shell_exit_cwd_timeout_and_output_bound() {
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    let shell = Shell::new(root.clone());
    let r = call(&shell, json!({"command":"pwd; printf problem >&2; exit 7"}))
        .await
        .unwrap();
    assert_eq!(r["ok"], false);
    assert_eq!(r["exit_code"], 7);
    assert_eq!(r["stderr"], "problem");
    assert!(r["stdout"]
        .as_str()
        .unwrap()
        .contains(root.to_str().unwrap()));
    let r = call(&shell, json!({"command":"sleep 30","timeout_ms":30}))
        .await
        .unwrap();
    assert_eq!(r["termination"], "timeout");
    assert_eq!(r["ok"], false);
    let r = call(&shell, json!({"command":"yes x"})).await.unwrap();
    assert_eq!(r["termination"], "output_limit");
    assert_eq!(r["output_complete"], false);
    assert!(r["stdout"].as_str().unwrap().len() <= MAX_OUTPUT_BYTES);
    let r = call(
        &shell,
        json!({"command":"export YOURAI_TEST_TEMP=hello; cd /"}),
    )
    .await
    .unwrap();
    assert_eq!(r["ok"], true);
    let r = call(
        &shell,
        json!({"command":"printf '%s' \"${YOURAI_TEST_TEMP-unset}\"; pwd"}),
    )
    .await
    .unwrap();
    assert!(r["stdout"].as_str().unwrap().starts_with("unset"));
    assert!(r["stdout"]
        .as_str()
        .unwrap()
        .contains(root.to_str().unwrap()));
}
#[cfg(unix)]
#[tokio::test]
async fn dropping_shell_future_kills_descendants() {
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    let shell = Arc::new(Shell::new(root.clone()));
    let cancel = CancellationToken::new();
    let task = tokio::spawn(async move {
        shell
            .execute(
                ToolContext {
                    call_id: "drop".into(),
                    emit: &DiscardSink,
                    cancel: &cancel,
                    security: None,
                    sandbox: None,
                    interaction: None,
                },
                json!({"command":"(sleep 0.4; echo leaked > marker) & echo ready > ready; wait"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !root.join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    task.abort();
    let _ = task.await;
    tokio::time::sleep(Duration::from_millis(550)).await;
    assert!(
        !root.join("marker").exists(),
        "descendant outlived dropped execute future"
    );
}
struct Deny;
impl SecurityProvider for Deny {
    fn check_tool_call<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async { Ok(ApprovalDecision::Deny) })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Deny) })
    }
    fn check_file_access<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Deny) })
    }
}
#[tokio::test]
async fn hard_denial_prevents_side_effects() {
    let d = TempDir::new().unwrap();
    let cancel = CancellationToken::new();
    let write = Write::new(d.path().into());
    assert!(write
        .execute(
            ToolContext {
                call_id: "deny".into(),
                emit: &DiscardSink,
                cancel: &cancel,
                security: Some(Arc::new(Deny)),
                sandbox: None,
                interaction: None
            },
            json!({"path":"blocked","content":"bad"})
        )
        .await
        .is_err());
    assert!(!d.path().join("blocked").exists());
}

#[tokio::test]
async fn oversized_writes_leave_original_unchanged_and_tools_have_unique_schemas() {
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    std::fs::write(root.join("a"), "original").unwrap();
    let write = Write::new(root.clone());
    assert!(call(
        &write,
        json!({"path":"a","content":"x".repeat(MAX_FILE_BYTES+1)})
    )
    .await
    .is_err());
    assert_eq!(std::fs::read_to_string(root.join("a")).unwrap(), "original");
    let tools = coding_tools(&root).unwrap();
    let names: std::collections::HashSet<_> = tools.iter().map(|t| t.name()).collect();
    assert_eq!(names.len(), 4);
    for t in &tools {
        assert_eq!(t.name(), t.definition().name.as_str());
    }
}
#[cfg(unix)]
#[tokio::test]
async fn cancellation_token_stops_shell_and_sandbox_denial_prevents_spawn() {
    let d = TempDir::new().unwrap();
    let root = d.path().canonicalize().unwrap();
    let cancel = CancellationToken::new();
    let token = cancel.clone();
    let shell = Shell::new(root.clone());
    let task = tokio::spawn(async move {
        shell
            .execute(
                ToolContext {
                    call_id: "cancel".into(),
                    emit: &DiscardSink,
                    cancel: &token,
                    security: None,
                    sandbox: None,
                    interaction: None,
                },
                json!({"command":"echo ready > ready; sleep 30; echo leaked > leak"}),
            )
            .await
    });
    tokio::time::timeout(Duration::from_secs(2), async {
        while !root.join("ready").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    cancel.cancel();
    let result = tokio::time::timeout(Duration::from_secs(1), task)
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(
        result,
        Err(YourAiError::Aborted(AbortReason::Cancelled))
    ));
    assert!(!root.join("leak").exists());
    struct Sandbox;
    impl SandboxProvider for Sandbox {
        fn policy(&self) -> SandboxPolicy {
            SandboxPolicy::ReadOnly {
                network_access: false,
            }
        }
        fn sandbox_type(&self) -> SandboxType {
            SandboxType::None
        }
        fn is_active(&self) -> bool {
            true
        }
        fn apply(&self, _: &mut tokio::process::Command) -> Result<(), YourAiError> {
            Err(ErrorKind::Config("sandbox denied".into()).into())
        }
    }
    let cancel = CancellationToken::new();
    let shell = Shell::new(root.clone());
    assert!(shell
        .execute(
            ToolContext {
                call_id: "sandbox".into(),
                emit: &DiscardSink,
                cancel: &cancel,
                security: None,
                sandbox: Some(Arc::new(Sandbox)),
                interaction: None
            },
            json!({"command":"echo bad > blocked"})
        )
        .await
        .is_err());
    assert!(!root.join("blocked").exists());
}
