//! Public operation wrappers own hooks and effect application.
use super::*;

/// Fixed core template: public operations own hooks and call private run methods.
impl Workspace {
    pub async fn setup(&self, trigger: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
        let result = host
            .dispatch_active(HookEvent::Setup {
                trigger: trigger.into(),
            })
            .await?;
        result.ensure_allowed()?;
        let _gate = gate;
        let _activity = activity;
        host.apply_hook(&result).await
    }
    pub async fn load_instructions(&self, path: &Path, reason: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
        let path = resolve(&host.context().cwd, path)?;
        let content = std::fs::read_to_string(&path).map_err(|e| error("instructions", e))?;
        let result = host
            .dispatch_active(HookEvent::InstructionsLoaded {
                file_path: path.to_string_lossy().into_owned(),
                memory_type: "project".into(),
                load_reason: reason.into(),
                globs: None,
                trigger_file_path: None,
                parent_file_path: None,
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        self.run_load_instructions(host, path, content, gate, activity)
            .await
    }
    pub async fn notify(&self, message: &str, kind: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let result = host
            .dispatch_active(HookEvent::Notification {
                message: message.into(),
                title: None,
                notification_type: kind.into(),
            })
            .await?;
        host.apply_hook(&result).await?;
        let _activity = activity;
        self.run_notify(&host, message).await
    }
    pub async fn change_config(&self, source: &str, value: Value) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
        let config: RuntimeConfig =
            serde_json::from_value(value.clone()).map_err(|e| error("config", e))?;
        let candidate = host.dir.join("config.candidate.json");
        atomic_write(&candidate, &value)?;
        let _candidate = CandidateFile(candidate.clone());
        let result = host
            .dispatch_active(HookEvent::ConfigChange {
                source: source.into(),
                file_path: Some(candidate.to_string_lossy().into_owned()),
            })
            .await;
        let result = result?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        self.run_config(&host, config, value, gate, activity).await
    }
    pub async fn change_cwd(&self, path: &Path) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
        let old = host.context().cwd;
        let new = resolve(&old, path)?;
        if !new.is_dir() {
            return Err(error("workspace", "cwd is not a directory"));
        }
        let result = host
            .dispatch_active(HookEvent::CwdChanged {
                old_cwd: old.to_string_lossy().into_owned(),
                new_cwd: new.to_string_lossy().into_owned(),
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        let paths = match result.outcome {
            HookPointOutcome::CwdChanged(o) => o.watch_paths,
            _ => vec![],
        };
        host.blocking(move |host| {
            let _gate = gate;
            let _activity = activity;
            host.set_cwd_with_watch(new, paths)
        })
        .await??;
        if !host.closing().is_cancelled() {
            host.workspace()?
                .start_watching(Duration::from_millis(250))?;
        }
        Ok(())
    }
    pub async fn create_worktree(&self, name: &str) -> Result<PathBuf, YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
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
            .dispatch_active(HookEvent::WorktreeCreate { name: name.into() })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        let custom = match result.outcome {
            HookPointOutcome::WorktreeCreate(o) => o.worktree_path,
            _ => None,
        };
        self.run_create_worktree(host, name.into(), custom, gate, activity)
            .await
    }
    pub async fn remove_worktree(&self, name: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let activity = host.activity()?;
        let gate = host.try_operation()?;
        let registration = self
            .worktrees
            .lock()
            .unwrap()
            .get(name)
            .cloned()
            .ok_or_else(|| error("workspace", "unknown worktree"))?;
        let result = host
            .dispatch_active(HookEvent::WorktreeRemove {
                worktree_path: registration.path().to_string_lossy().into_owned(),
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        self.run_remove_worktree(host, name.into(), registration, gate, activity)
            .await
    }
    pub(super) async fn file_changed(&self, host: &Arc<SessionHost>, path: &Path, event: &str) {
        let Ok(_activity) = host.activity() else {
            return;
        };
        let mut errors = vec![];
        match host
            .dispatch_active(HookEvent::FileChanged {
                file_path: path.to_string_lossy().into_owned(),
                event: event.into(),
            })
            .await
        {
            Err(cause) => errors.push(format!("FileChanged hook failed: {cause}")),
            Ok(result) => {
                errors.extend(result.blocking_messages());
                if let Err(cause) = host.apply_hook(&result).await {
                    errors.push(format!("FileChanged effects failed: {cause}"));
                }
                if let HookPointOutcome::FileChanged(outcome) = result.outcome {
                    for path in outcome.watch_paths {
                        if let Err(cause) = host.watch_path_async(PathBuf::from(path)).await {
                            errors.push(format!("FileChanged watch registration failed: {cause}"));
                        }
                    }
                }
            }
        }
        if let Err(cause) = host
            .post_event_async(RuntimeEvent {
                id: uuid::Uuid::new_v4().to_string(),
                context: Some(format!("File {event}: {}", path.display())),
                notice: (!errors.is_empty()).then(|| errors.join("\n")),
                wake: false,
            })
            .await
        {
            host.close_warning(&error(
                "workspace",
                format!("FileChanged event save failed: {cause}"),
            ));
        }
    }
}
struct CandidateFile(PathBuf);
impl Drop for CandidateFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
    }
}
