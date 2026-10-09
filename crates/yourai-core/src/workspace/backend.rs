//! Private business effects; public operations decide their hook lifecycle.
use super::*;
use crate::runtime::Activity;

impl Workspace {
    pub(super) async fn run_load_instructions(
        &self,
        host: Arc<SessionHost>,
        path: PathBuf,
        content: String,
        gate: tokio::sync::OwnedMutexGuard<()>,
        activity: Activity,
    ) -> Result<(), YourAiError> {
        tokio::spawn(async move {
            let _gate = gate;
            let _activity = activity;
            host.ensure_open()?;
            let history = host.agent().ctx().context_manager()?;
            let prefix = format!("[Instructions: {}]\n", path.display());
            let note = format!("{prefix}{content}");
            let indexed = host.context().instructions.get(&path) == Some(&content);
            // Only the latest value for this path is current. A historical A
            // must not suppress a real B -> A update; it can suppress a retry
            // whose append succeeded before the in-memory index was published.
            let recorded = history
                .records()
                .iter()
                .rev()
                .find(|row| {
                    row.runtime_context
                        && row
                            .message
                            .content
                            .first_text()
                            .is_some_and(|text| text.starts_with(&prefix))
                })
                .is_some_and(|row| row.message.content.first_text() == Some(note.as_str()));
            if !indexed && !recorded {
                history
                    .append(vec![StoredMessage::runtime_context(note)])
                    .await?;
            }
            host.context
                .lock()
                .unwrap()
                .instructions
                .insert(path.clone(), content);
            if !host.closing().is_cancelled() {
                host.watch_path_async(path).await?;
            }
            Ok(())
        })
        .await
        .map_err(|e| error("instructions", e))?
    }
    pub(super) async fn run_notify(
        &self,
        host: &Arc<SessionHost>,
        message: &str,
    ) -> Result<(), YourAiError> {
        host.post_event_async(RuntimeEvent {
            id: uuid::Uuid::new_v4().to_string(),
            context: None,
            notice: Some(message.into()),
            wake: false,
        })
        .await?;
        Ok(())
    }
    pub(super) async fn run_config(
        &self,
        host: &Arc<SessionHost>,
        config: RuntimeConfig,
        value: Value,
        gate: tokio::sync::OwnedMutexGuard<()>,
        activity: Activity,
    ) -> Result<(), YourAiError> {
        host.blocking(move |host| {
            let _gate = gate;
            let _activity = activity;
            let _write = host.resource_lock()?;
            atomic_write(&host.dir.join("config.json"), &value)?;
            config.apply(&host);
            Ok(())
        })
        .await?
    }
    pub(super) async fn run_create_worktree(
        &self,
        host: Arc<SessionHost>,
        name: String,
        custom: Option<String>,
        gate: tokio::sync::OwnedMutexGuard<()>,
        activity: Activity,
    ) -> Result<PathBuf, YourAiError> {
        let workspace = host.workspace()?;
        tokio::spawn(async move {
            let operation_gate = gate;
            let operation_activity = activity;
            host.ensure_open()?;
            let cwd = host.context().cwd;
            let repository = common_dir(&cwd).await?;
            let path = if let Some(path) = custom {
                let path = PathBuf::from(path).canonicalize().map_err(|e| error("workspace", e))?;
                verify_worktree(&path, Some(&repository)).await?;
                path
            } else {
                let path = host.dir.join("worktrees").join(&name);
                std::fs::create_dir_all(path.parent().unwrap()).map_err(|e| error("workspace", e))?;
                // A previous git success may have outlived a failed registration.
                // Verify and adopt that exact tree instead of repeating the effect.
                if path.exists() {
                    verify_worktree(&path, Some(&repository)).await?;
                } else {
                    git(&cwd, &["worktree", "add", "--detach", utf8(&path)?, "HEAD"]).await?;
                }
                path.canonicalize().map_err(|e| error("workspace", e))?
            };
            host.blocking(move |host| {
                let _gate = operation_gate;
                let _activity = operation_activity;
                let _write = host.resource_settlement()?;
                let mut map = workspace.worktrees.lock().unwrap();
                if map.values().any(|tracked| tracked.path() == path) {
                    return Err(error("workspace", "worktree path already tracked"));
                }
                let mut next = map.clone();
                next.insert(name, WorktreeRegistration::Known {
                    path: path.clone(), repository,
                });
                atomic_write(&host.dir.join("worktrees.json"), &next)
                    .map_err(|e| error("workspace", format!("git worktree exists at {}; registration was not confirmed: {e}; verify before retrying", path.display())))?;
                *map = next;
                Ok(path)
            }).await?
        }).await.map_err(|e| error("workspace", e))?
    }
    pub(super) async fn run_remove_worktree(
        &self,
        host: Arc<SessionHost>,
        name: String,
        registration: WorktreeRegistration,
        gate: tokio::sync::OwnedMutexGuard<()>,
        activity: Activity,
    ) -> Result<(), YourAiError> {
        let workspace = host.workspace()?;
        tokio::spawn(async move {
            let operation_gate = gate;
            let operation_activity = activity;
            host.ensure_open()?;
            let path = registration.path().to_owned();
            let repository = if path.exists() {
                verify_worktree(&path, registration.repository()).await?
            } else if let Some(repository) = registration.repository() {
                repository.to_owned()
            } else {
                // Older path-only registrations can still be repaired if the
                // current repository confirms ownership of the missing path.
                let repository = common_dir(&host.context().cwd).await?;
                if !registered_worktree(&repository, &path).await? {
                    return Err(error("workspace", "missing legacy worktree has no confirmed repository; restore its directory or repair its registration"));
                }
                repository
            };
            if registered_worktree(&repository, &path).await? {
                git(&repository, &["worktree", "remove", utf8(&path)?]).await?;
            }
            host.blocking(move |host| {
                let _gate = operation_gate;
                let _activity = operation_activity;
                let _write = host.resource_settlement()?;
                let mut map = workspace.worktrees.lock().unwrap();
                let mut next = map.clone();
                next.remove(&name);
                atomic_write(&host.dir.join("worktrees.json"), &next)
                    .map_err(|e| error("workspace", format!("git worktree removed at {}; registration removal was not confirmed: {e}; verify before retrying", path.display())))?;
                *map = next;
                Ok(())
            }).await?
        }).await.map_err(|e| error("workspace", e))?
    }
}
fn utf8(path: &Path) -> Result<&str, YourAiError> {
    path.to_str()
        .ok_or_else(|| error("workspace", "non UTF-8 worktree path"))
}
async fn common_dir(path: &Path) -> Result<PathBuf, YourAiError> {
    let output = git(
        path,
        &["rev-parse", "--path-format=absolute", "--git-common-dir"],
    )
    .await?;
    PathBuf::from(
        String::from_utf8(output)
            .map_err(|e| error("git", e))?
            .trim_end_matches(['\n', '\r']),
    )
    .canonicalize()
    .map_err(|e| error("git", e))
}
async fn verify_worktree(path: &Path, expected: Option<&Path>) -> Result<PathBuf, YourAiError> {
    if !path.is_dir() || !path.join(".git").is_file() {
        return Err(error("workspace", "path is not a linked git worktree"));
    }
    let repository = common_dir(path).await?;
    if expected.is_some_and(|expected| expected != repository) {
        return Err(error(
            "workspace",
            "hook worktree belongs to another repository",
        ));
    }
    let canonical = path.canonicalize().map_err(|e| error("workspace", e))?;
    if !registered_worktree(&repository, &canonical).await? {
        return Err(error(
            "workspace",
            "path is not registered in git worktree list",
        ));
    }
    Ok(repository)
}
async fn registered_worktree(repository: &Path, path: &Path) -> Result<bool, YourAiError> {
    let list = git(repository, &["worktree", "list", "--porcelain", "-z"]).await?;
    Ok(list.split(|byte| *byte == 0).any(|entry| {
        entry
            .strip_prefix(b"worktree ")
            .and_then(|bytes| std::str::from_utf8(bytes).ok())
            .is_some_and(|registered| Path::new(registered) == path)
    }))
}
