//! Public operation wrappers own hooks and effect application.
use super::*;

/// 缝实现：工作区变更生命周期在本实现内（宿主 gate / 监视集 / 事件队列），
/// 公共入口 `yourai_core::workspace::*` 委托到这里。
impl yourai_core::workspace::WorkspaceOperation for Workspace {
    fn setup_bound<'a>(&'a self, trigger: &'a str) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::setup(self, trigger))
    }
    fn load_instructions_bound<'a>(
        &'a self,
        path: &'a Path,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::load_instructions(self, path, reason))
    }
    fn notify_bound<'a>(
        &'a self,
        message: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::notify(self, message, kind))
    }
    fn change_config_bound<'a>(
        &'a self,
        source: &'a str,
        value: Value,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::change_config(self, source, value))
    }
    fn change_cwd_bound<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::change_cwd(self, path))
    }
    fn create_worktree_bound<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, YourAiError>> {
        Box::pin(Workspace::create_worktree(self, name))
    }
    fn remove_worktree_bound<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(Workspace::remove_worktree(self, name))
    }
    fn file_changed_bound<'a>(&'a self, path: &'a Path, event: &'a str) -> BoxFuture<'a, ()> {
        Box::pin(async move {
            let Ok(host) = self.host() else { return };
            Workspace::file_changed(self, &host, path, event).await
        })
    }
}

impl Workspace {
    pub async fn setup(&self, trigger: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let result = host
            .dispatch(HookEvent::Setup {
                trigger: trigger.into(),
            })
            .await?;
        host.consume_hook_async(&result, true).await
    }
    pub async fn load_instructions(&self, path: &Path, reason: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        self.load_inner(&host, path, reason).await
    }
    async fn load_inner(
        &self,
        host: &Arc<SessionHost>,
        path: &Path,
        reason: &str,
    ) -> Result<(), YourAiError> {
        let path = resolve(&host.context().cwd, path)?;
        let content = std::fs::read_to_string(&path).map_err(|e| error("instructions", e))?;
        let result = host
            .dispatch(HookEvent::InstructionsLoaded {
                file_path: path.to_string_lossy().into_owned(),
                memory_type: "project".into(),
                load_reason: reason.into(),
                globs: None,
                trigger_file_path: None,
                parent_file_path: None,
            })
            .await?;
        host.consume_hook_async(&result, false).await?;
        self.run_load_instructions(host, path, content).await
    }
    pub async fn notify(&self, message: &str, kind: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let result = host
            .dispatch(HookEvent::Notification {
                message: message.into(),
                title: None,
                notification_type: kind.into(),
            })
            .await?;
        host.consume_hook_async(&result, false).await?;
        self.run_notify(&host, message).await
    }
    pub async fn change_config(&self, source: &str, value: Value) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let config: RuntimeConfig =
            serde_json::from_value(value.clone()).map_err(|e| error("config", e))?;
        let candidate = host.dir.join("config.candidate.json");
        atomic_write(&candidate, &value)?;
        let result = host
            .dispatch(HookEvent::ConfigChange {
                source: source.into(),
                file_path: Some(candidate.to_string_lossy().into_owned()),
            })
            .await;
        let _ = std::fs::remove_file(candidate);
        host.consume_hook_async(&result?, true).await?;
        self.run_config(&host, config, &value)
    }
    pub async fn change_cwd(&self, path: &Path) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let old = host.context().cwd;
        let new = resolve(&old, path)?;
        if !new.is_dir() {
            return Err(error("workspace", "cwd is not a directory"));
        }
        let result = host
            .dispatch(HookEvent::CwdChanged {
                old_cwd: old.to_string_lossy().into_owned(),
                new_cwd: new.to_string_lossy().into_owned(),
            })
            .await?;
        host.consume_hook_async(&result, true).await?;
        if let HookPointOutcome::CwdChanged(o) = result.outcome {
            for p in o.watch_paths {
                host.watch_path_async(PathBuf::from(p)).await?;
            }
        }
        self.run_cwd(&host, new).await
    }
    pub async fn create_worktree(&self, name: &str) -> Result<PathBuf, YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        {
            return Err(error("workspace", "invalid worktree name"));
        }
        if self.worktrees.lock().unwrap().contains_key(name) {
            return Err(error("workspace", "worktree already tracked"));
        }
        let result = host
            .dispatch(HookEvent::WorktreeCreate { name: name.into() })
            .await?;
        host.consume_hook_async(&result, true).await?;
        let custom = match result.outcome {
            HookPointOutcome::WorktreeCreate(o) => o.worktree_path,
            _ => None,
        };
        self.run_create_worktree(&host, name, custom).await
    }
    pub async fn remove_worktree(&self, name: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let path = self
            .worktrees
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| error("workspace", "unknown worktree"))?;
        let result = host
            .dispatch(HookEvent::WorktreeRemove {
                worktree_path: path.to_string_lossy().into_owned(),
            })
            .await?;
        host.consume_hook_async(&result, true).await?;
        self.run_remove_worktree(&host, name, &path).await
    }
    pub(super) async fn file_changed(&self, host: &Arc<SessionHost>, path: &Path, event: &str) {
        let result = host
            .dispatch(HookEvent::FileChanged {
                file_path: path.to_string_lossy().into_owned(),
                event: event.into(),
            })
            .await;
        if let Ok(r) = result {
            let _ = host.consume_hook_async(&r, false).await;
            if let HookPointOutcome::FileChanged(o) = r.outcome {
                for p in o.watch_paths {
                    let _ = host.watch_path_async(PathBuf::from(p)).await;
                }
            }
        }
        let _ = host
            .post_event_async(RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: Some(format!("File {event}: {}", path.display())),
                notice: None,
                wake: false,
            })
            .await;
    }
}
