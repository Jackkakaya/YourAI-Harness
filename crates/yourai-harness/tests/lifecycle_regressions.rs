#[path = "support/loop.rs"]
mod support;

use serde_json::{json, Value};
use std::{
    path::Path,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    },
};
use support::{answer, call, end, events, Hooks, Model};
use tempfile::TempDir;
use tokio_util::sync::CancellationToken;
use yourai_core::{
    context::DiscardSink,
    hooks::{PermissionRequestBehavior, PermissionRequestDecision},
    prelude::*,
};
use yourai_harness::{collaboration::subagent, runtime, SessionHost};

async fn open(root: &Path, cwd: &Path, model: Arc<Model>) -> Arc<SessionHost> {
    runtime::create(root, cwd.to_owned(), model, None, None)
        .await
        .unwrap()
}
async fn turn(host: &SessionHost) {
    host.submit_async(In::user_text("continue")).await.unwrap();
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
}
fn tool_call(id: &str, name: &str, input: Value) -> ToolCall {
    let mut call = call(id, name);
    call.fn_arguments = input;
    call
}

#[derive(Default)]
struct Security {
    deny_reads: bool,
    approvals: Mutex<Vec<SecurityContext>>,
    files: Mutex<Vec<String>>,
}
impl SecurityProvider for Security {
    fn check_tool_call<'a>(
        &'a self,
        ctx: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async move {
            self.approvals.lock().unwrap().push(ctx.clone());
            Ok(ApprovalDecision::Allow)
        })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Allow) })
    }
    fn check_file_access<'a>(
        &'a self,
        path: &'a str,
        write: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async move {
            self.files.lock().unwrap().push(path.into());
            Ok(if self.deny_reads && !write {
                PolicyDecision::Deny
            } else {
                PolicyDecision::Allow
            })
        })
    }
}
struct RejectSandbox(AtomicUsize);
impl SandboxProvider for RejectSandbox {
    fn policy(&self) -> SandboxPolicy {
        SandboxPolicy::ExternalSandbox {
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
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ErrorKind::Config("sandbox rejected child command".into()).into())
    }
}

#[tokio::test]
async fn child_inherits_normal_security_and_sandbox() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    std::fs::write(cwd.path().join("secret.txt"), "TOP-SECRET-CONTENT").unwrap();
    let parent = open(root.path(), cwd.path(), Arc::new(Model::new(vec![]))).await;
    let security = Arc::new(Security {
        deny_reads: true,
        ..Default::default()
    });
    let sandbox = Arc::new(RejectSandbox(AtomicUsize::new(0)));
    parent.agent().ctx().set_security(security.clone());
    parent.agent().ctx().set_sandbox(sandbox.clone());
    let replacement_security = Arc::new(Security::default());
    let replacement_sandbox = Arc::new(RejectSandbox(AtomicUsize::new(0)));
    let startup_parent = Arc::downgrade(&parent);
    let startup_security = replacement_security.clone();
    let startup_sandbox = replacement_sandbox.clone();
    let hooks = Arc::new(Hooks::new(move |invocation, result| {
        if invocation.event.kind() == HookEventKind::SessionStart {
            // Startup can change the parent, but this child must retain the
            // providers captured by the tool invocation that created it.
            let parent = startup_parent.upgrade().unwrap();
            parent.agent().ctx().set_security(startup_security.clone());
            parent.agent().ctx().set_sandbox(startup_sandbox.clone());
        }
        if let HookPointOutcome::PermissionRequest(outcome) = &mut result.outcome {
            outcome.decision = Some(PermissionRequestDecision {
                behavior: PermissionRequestBehavior::Allow,
                updated_input: None,
                updated_permissions: vec![],
                message: None,
                interrupt: false,
            });
        }
    }));
    parent.agent().ctx().set_hooks(hooks.clone());
    let model = Arc::new(Model::new(vec![
        events(vec![end(
            "",
            vec![
                tool_call("read-child", "read", json!({"path":"secret.txt"})),
                tool_call(
                    "shell-child",
                    "shell",
                    json!({"command":"printf UNCONTAINED-CHILD"}),
                ),
            ],
        )]),
        answer("done"),
    ]));
    let child_tool = subagent(&parent, Some(model.clone()), None);
    child_tool
        .exec(
            ToolContext {
                providers: None,
                call_id: "child".into(),
                cwd: Some(cwd.path()),
                emit: &DiscardSink,
                cancel: &CancellationToken::new(),
                security: Some(security.clone()),
                sandbox: Some(sandbox.clone()),
                interaction: None,
            },
            "inspect files".into(),
        )
        .await
        .unwrap();
    assert_eq!(security.files.lock().unwrap().len(), 1);
    assert_eq!(sandbox.0.load(Ordering::SeqCst), 1);
    assert!(replacement_security.files.lock().unwrap().is_empty());
    assert!(replacement_security.approvals.lock().unwrap().is_empty());
    assert_eq!(replacement_sandbox.0.load(Ordering::SeqCst), 0);
    let text = model.requests.lock().unwrap()[1]
        .request
        .messages
        .iter()
        .flat_map(|message| {
            message
                .content
                .tool_responses()
                .into_iter()
                .map(|response| response.content.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("file access denied by hard policy"));
    assert!(text.contains("sandbox rejected child command"));
    assert!(!text.contains("TOP-SECRET-CONTENT"));
    assert!(!text.contains("UNCONTAINED-CHILD"));
    assert!(hooks
        .seen
        .lock()
        .unwrap()
        .contains(&HookEventKind::SessionStart));
    parent.close(None).await.unwrap();
}

#[tokio::test]
async fn changed_cwd_is_shared_by_execution_and_approval_paths() {
    let root = TempDir::new().unwrap();
    let old = TempDir::new().unwrap();
    let new = TempDir::new().unwrap();
    std::fs::write(old.path().join("marker"), "OLD-CWD-CONTENT").unwrap();
    std::fs::write(new.path().join("marker"), "NEW-CWD-CONTENT").unwrap();
    std::fs::write(old.path().join("edit.txt"), "OLD-EDIT").unwrap();
    std::fs::write(new.path().join("edit.txt"), "BEFORE").unwrap();
    let model = Arc::new(Model::new(vec![
        events(vec![end(
            "",
            vec![
                tool_call("read", "read", json!({"path":"marker"})),
                tool_call(
                    "write",
                    "write",
                    json!({"path":"created.txt","content":"CREATED"}),
                ),
                tool_call(
                    "edit",
                    "edit",
                    json!({"path":"edit.txt","old_text":"BEFORE","new_text":"AFTER"}),
                ),
                tool_call("shell", "shell", json!({"command":"pwd"})),
            ],
        )]),
        answer("done"),
    ]));
    let host = open(root.path(), old.path(), model.clone()).await;
    host.workspace()
        .unwrap()
        .change_cwd(new.path())
        .await
        .unwrap();
    let cwd = new.path().canonicalize().unwrap();
    // Cwd changes execution defaults, without widening the policy's original
    // workspace. The approval description now correctly points outside it.
    let read = yourai_harness::tools::Read::new(old.path().canonicalize().unwrap());
    let description = read.security_context(&json!({"path":"marker"}), Some(&cwd));
    assert_eq!(
        host.agent()
            .ctx()
            .security()
            .unwrap()
            .check_tool_call(&description)
            .await
            .unwrap(),
        ApprovalDecision::Ask
    );
    let security = Arc::new(Security::default());
    host.agent().ctx().set_security(security.clone());
    turn(&host).await;
    let text = model.requests.lock().unwrap()[1]
        .request
        .messages
        .iter()
        .flat_map(|message| {
            message
                .content
                .tool_responses()
                .into_iter()
                .map(|response| response.content.as_str())
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(text.contains("NEW-CWD-CONTENT"));
    assert!(!text.contains("OLD-CWD-CONTENT"));
    assert!(text.contains(cwd.to_str().unwrap()));
    assert_eq!(
        std::fs::read_to_string(new.path().join("created.txt")).unwrap(),
        "CREATED"
    );
    assert!(!old.path().join("created.txt").exists());
    assert_eq!(
        std::fs::read_to_string(new.path().join("edit.txt")).unwrap(),
        "AFTER"
    );
    assert_eq!(
        std::fs::read_to_string(old.path().join("edit.txt")).unwrap(),
        "OLD-EDIT"
    );
    let approvals = security.approvals.lock().unwrap().clone();
    assert_eq!(approvals.len(), 4);
    for approval in approvals.iter() {
        let field = if approval.action == "shell" {
            "cwd"
        } else {
            "path"
        };
        assert!(Path::new(approval.input[field].as_str().unwrap()).starts_with(&cwd));
    }
    drop(approvals);
    assert_eq!(security.files.lock().unwrap().len(), 3);
    host.close(None).await.unwrap();
}

#[tokio::test]
async fn instructions_reverting_to_old_content_publish_the_latest_value() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let model = Arc::new(Model::new(vec![answer("done")]));
    let host = open(root.path(), cwd.path(), model.clone()).await;
    let path = cwd.path().join("DYNAMIC.md");
    let workspace = host.workspace().unwrap();
    for content in ["A-CONTENT", "B-CONTENT", "A-CONTENT", "A-CONTENT"] {
        std::fs::write(&path, content).unwrap();
        workspace.load_instructions(&path, "include").await.unwrap();
    }
    assert_eq!(
        host.context().instructions[&path.canonicalize().unwrap()],
        "A-CONTENT"
    );
    turn(&host).await;
    let texts: Vec<_> = model.requests.lock().unwrap()[0]
        .request
        .messages
        .iter()
        .filter_map(|message| message.content.first_text().map(str::to_owned))
        .filter(|text| text.starts_with("[Instructions: "))
        .collect();
    assert_eq!(texts.len(), 3);
    assert!(texts[0].ends_with("A-CONTENT"));
    assert!(texts[1].ends_with("B-CONTENT"));
    assert!(texts[2].ends_with("A-CONTENT"));
    host.close(None).await.unwrap();
}

fn git(cwd: &Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(cwd)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}
fn initialize_repo(path: &Path) {
    git(path, &["init"]);
    git(
        path,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "initial",
        ],
    );
}

#[tokio::test]
async fn worktree_created_from_linked_tree_uses_its_head() {
    let root = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    initialize_repo(repo.path());
    let linked = repo.path().join("linked");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    git(
        &linked,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.test",
            "commit",
            "--allow-empty",
            "-m",
            "linked-only",
        ],
    );
    let linked_head = git(&linked, &["rev-parse", "HEAD"]);
    assert_ne!(linked_head, git(repo.path(), &["rev-parse", "HEAD"]));
    let host = open(root.path(), &linked, Arc::new(Model::new(vec![]))).await;
    let workspace = host.workspace().unwrap();
    let created = workspace.create_worktree("fork").await.unwrap();
    assert_eq!(git(&created, &["rev-parse", "HEAD"]), linked_head);
    workspace.remove_worktree("fork").await.unwrap();
    host.close(None).await.unwrap();
}

#[tokio::test]
async fn missing_worktree_directory_is_removed_from_its_original_repository() {
    let root = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    let other = TempDir::new().unwrap();
    initialize_repo(repo.path());
    initialize_repo(other.path());
    let host = open(root.path(), repo.path(), Arc::new(Model::new(vec![]))).await;
    let workspace = host.workspace().unwrap();
    let created = workspace.create_worktree("gone").await.unwrap();
    std::fs::remove_dir_all(&created).unwrap();
    workspace.change_cwd(other.path()).await.unwrap();
    workspace.remove_worktree("gone").await.unwrap();
    assert!(
        !git(repo.path(), &["worktree", "list", "--porcelain"]).contains(created.to_str().unwrap())
    );
    workspace.change_cwd(repo.path()).await.unwrap();
    assert_eq!(workspace.create_worktree("gone").await.unwrap(), created);
    workspace.remove_worktree("gone").await.unwrap();
    host.close(None).await.unwrap();
}

#[tokio::test]
async fn legacy_path_only_worktree_registration_can_remove_a_missing_directory() {
    let root = TempDir::new().unwrap();
    let repo = TempDir::new().unwrap();
    initialize_repo(repo.path());
    let host = open(root.path(), repo.path(), Arc::new(Model::new(vec![]))).await;
    let linked = repo.path().join("legacy");
    git(
        repo.path(),
        &[
            "worktree",
            "add",
            "--detach",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let linked = linked.canonicalize().unwrap();
    std::fs::write(
        host.directory().join("worktrees.json"),
        serde_json::to_vec(&json!({"legacy":linked})).unwrap(),
    )
    .unwrap();
    std::fs::remove_dir_all(&linked).unwrap();
    host.workspace()
        .unwrap()
        .remove_worktree("legacy")
        .await
        .unwrap();
    assert!(
        !git(repo.path(), &["worktree", "list", "--porcelain"]).contains(linked.to_str().unwrap())
    );
    host.close(None).await.unwrap();
}

#[tokio::test]
async fn imported_task_backup_is_retired_even_when_it_changes_or_breaks() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let host = open(root.path(), cwd.path(), Arc::new(Model::new(vec![]))).await;
    let id = host.context().id;
    let legacy = host.directory().join("tasks.json");
    std::fs::write(&legacy, r#"{"a":{"id":"a","subject":"imported","description":null,"owner":null,"completed":false}}"#).unwrap();
    host.task_manager("team")
        .unwrap()
        .complete("a")
        .await
        .unwrap();
    assert!(legacy.exists());
    host.close(None).await.unwrap();
    for backup in [
        "broken backup",
        r#"{"b":{"id":"b","subject":"late backup task","description":null,"owner":null,"completed":false}}"#,
    ] {
        std::fs::write(&legacy, backup).unwrap();
        let restored = runtime::restore(
            root.path(),
            id.clone(),
            cwd.path().into(),
            Arc::new(Model::new(vec![])),
            None,
            None,
            None,
            None,
            ContextPolicy::default(),
            "resume",
        )
        .await
        .unwrap();
        let tasks = restored.task_manager("team").unwrap().list();
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].id, "a");
        assert!(tasks[0].completed);
        restored.close(None).await.unwrap();
    }
}

#[tokio::test]
async fn task_import_can_retry_after_completion_marker_write_fails() {
    let root = TempDir::new().unwrap();
    let cwd = TempDir::new().unwrap();
    let host = open(root.path(), cwd.path(), Arc::new(Model::new(vec![]))).await;
    std::fs::write(host.directory().join("tasks.json"), r#"{"a":{"id":"a","subject":"imported","description":null,"owner":null,"completed":false}}"#).unwrap();
    let marker = host.directory().join("tasks.migrated.json");
    // The marker path is a directory: existence alone must not acknowledge an
    // import. This fails only after the database has accepted the task.
    std::fs::create_dir(&marker).unwrap();
    assert!(host.task_manager("team").is_err());
    assert_eq!(
        host.agent()
            .ctx()
            .session()
            .unwrap()
            .read_tasks(&host.context().id)
            .unwrap()
            .len(),
        1
    );
    std::fs::remove_dir(&marker).unwrap();
    let manager = host.task_manager("team").unwrap();
    assert_eq!(manager.list().len(), 1);
    assert!(marker.is_file());
    host.close(None).await.unwrap();
}
