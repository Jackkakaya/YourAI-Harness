//! Optional modules own their effects and invoke hooks at actual lifecycle boundaries.
use crate::{
    error,
    storage::{atomic_write, read_json},
    SessionHost,
};

#[derive(Default, Clone, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RuntimeConfig {
    pub system_prompt: Option<String>,
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
}
impl RuntimeConfig {
    pub(crate) fn apply(self, host: &SessionHost) {
        let config = crate::default_loop::LoopConfig {
            skill_ids: self.skill_ids,
            memory_search_limit: self.memory_search_limit,
            ..Default::default()
        };
        host.agent
            .ctx()
            .set_agent_loop(Arc::new(crate::default_loop::DefaultLoop::new(config)));
    }
}
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use tokio_util::sync::CancellationToken;
use yourai_core::{prelude::*, runtime_event::RuntimeEvent};

type WatchSnapshots = HashMap<PathBuf, HashMap<PathBuf, Vec<u8>>>;

pub struct Workspace {
    host: Weak<SessionHost>,
    worktrees: Mutex<HashMap<String, PathBuf>>,
    watcher: Mutex<Option<tokio::task::JoinHandle<()>>>,
    stop: CancellationToken,
    snapshots: Mutex<WatchSnapshots>,
}
impl Workspace {
    pub fn new(host: &Arc<SessionHost>) -> Result<Arc<Self>, YourAiError> {
        let p = host.dir.join("worktrees.json");
        let worktrees = if p.exists() {
            read_json(&p)?
        } else {
            HashMap::new()
        };
        Ok(Arc::new(Self {
            host: Arc::downgrade(host),
            worktrees: Mutex::new(worktrees),
            watcher: Mutex::new(None),
            stop: CancellationToken::new(),
            snapshots: Mutex::new(HashMap::new()),
        }))
    }
    fn host(&self) -> Result<Arc<SessionHost>, YourAiError> {
        self.host
            .upgrade()
            .ok_or_else(|| error("workspace", "session gone"))
    }
    pub async fn setup(&self, trigger: &str) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let result = host
            .dispatch(HookEvent::Setup {
                trigger: trigger.into(),
            })
            .await?;
        host.consume_hook(&result, true)
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
        host.consume_hook(&result, false)?;
        host.context.lock().unwrap().instructions.insert(
            path.clone(),
            format!("[Instructions: {}]\n{content}", path.display()),
        );
        host.watch_path(path)?;
        Ok(())
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
        host.consume_hook(&result, false)?;
        host.post_event(RuntimeEvent {
            id: uuid::Uuid::new_v4().to_string(),
            context: None,
            notice: Some(message.into()),
            wake: false,
        })?;
        Ok(())
    }
    pub async fn change_config(&self, source: &str, value: Value) -> Result<(), YourAiError> {
        let host = self.host()?;
        let _gate = host.try_operation()?;
        let config: RuntimeConfig =
            serde_json::from_value(value.clone()).map_err(|e| error("config", e))?;
        let path = host.dir.join("config.json");
        let candidate = host.dir.join("config.candidate.json");
        atomic_write(&candidate, &value)?;
        let result = host
            .dispatch(HookEvent::ConfigChange {
                source: source.into(),
                file_path: Some(candidate.to_string_lossy().into_owned()),
            })
            .await;
        let _ = std::fs::remove_file(candidate);
        host.consume_hook(&result?, true)?;
        atomic_write(&path, &value)?;
        config.apply(&host);
        Ok(())
    }
    pub fn config(&self) -> Result<Value, YourAiError> {
        let p = self.host()?.dir.join("config.json");
        if p.exists() {
            read_json(&p)
        } else {
            Ok(json!({}))
        }
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
        host.consume_hook(&result, true)?;
        if let HookPointOutcome::CwdChanged(o) = result.outcome {
            for p in o.watch_paths {
                host.watch_path(PathBuf::from(p))?;
            }
        }
        host.set_cwd(new)
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
        host.consume_hook(&result, true)?;
        let custom = match result.outcome {
            HookPointOutcome::WorktreeCreate(o) => o.worktree_path,
            _ => None,
        };
        let path = if let Some(path) = custom {
            let p = PathBuf::from(path)
                .canonicalize()
                .map_err(|e| error("workspace", e))?;
            if !p.is_dir() {
                return Err(error("workspace", "hook worktree path is not a directory"));
            }
            p
        } else {
            let p = host.dir.join("worktrees").join(name);
            std::fs::create_dir_all(p.parent().unwrap()).map_err(|e| error("workspace", e))?;
            git(
                &host.context().cwd,
                &[
                    "worktree",
                    "add",
                    "--detach",
                    p.to_str()
                        .ok_or_else(|| error("workspace", "non UTF-8 worktree path"))?,
                    "HEAD",
                ],
            )
            .await?;
            p.canonicalize().map_err(|e| error("workspace", e))?
        };
        let mut map = self.worktrees.lock().unwrap();
        map.insert(name.into(), path.clone());
        atomic_write(&host.dir.join("worktrees.json"), &*map)?;
        Ok(path)
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
        host.consume_hook(&result, true)?;
        git(
            &host.context().cwd,
            &[
                "worktree",
                "remove",
                path.to_str()
                    .ok_or_else(|| error("workspace", "non UTF-8 path"))?,
            ],
        )
        .await?;
        let mut map = self.worktrees.lock().unwrap();
        map.remove(name);
        atomic_write(&host.dir.join("worktrees.json"), &*map)
    }
    /// Polling watcher avoids OS-specific dependencies; only explicitly registered paths.
    pub fn start_watching(self: &Arc<Self>, interval: Duration) -> Result<(), YourAiError> {
        if interval.is_zero() {
            return Err(error("watcher", "zero interval"));
        }
        let mut task = self.watcher.lock().unwrap();
        for root in self.host()?.watch_paths() {
            let mut snapshots = self.snapshots.lock().unwrap();
            if let std::collections::hash_map::Entry::Vacant(entry) = snapshots.entry(root.clone())
            {
                entry.insert(file_snapshot(&root)?);
            }
        }
        if task.is_some() {
            return Ok(());
        }
        let weak = Arc::downgrade(self);
        let stop = self.stop.clone();
        *task = Some(tokio::spawn(async move {
            loop {
                tokio::select! {_=stop.cancelled()=>break,_=tokio::time::sleep(interval)=>{}}
                let Some(ws) = weak.upgrade() else { break };
                let Ok(host) = ws.host() else { break };
                if matches!(
                    host.status(),
                    SessionStatus::Closed | SessionStatus::Closing
                ) {
                    break;
                }
                for root in host.watch_paths() {
                    let current = match file_snapshot(&root) {
                        Ok(snapshot) => snapshot,
                        Err(e) => {
                            let _ = host.post_event(RuntimeEvent {
                                id: uuid::Uuid::new_v4().to_string(),
                                context: None,
                                notice: Some(format!("File watcher: {e}")),
                                wake: false,
                            });
                            continue;
                        }
                    };
                    let old = ws.snapshots.lock().unwrap().insert(root, current.clone());
                    if let Some(old) = old {
                        let paths: std::collections::HashSet<_> =
                            old.keys().chain(current.keys()).cloned().collect();
                        for path in paths {
                            if old.get(&path) == current.get(&path) {
                                continue;
                            }
                            let event = if !current.contains_key(&path) {
                                "deleted"
                            } else if !old.contains_key(&path) {
                                "created"
                            } else {
                                "modified"
                            };
                            let result = host
                                .dispatch(HookEvent::FileChanged {
                                    file_path: path.to_string_lossy().into_owned(),
                                    event: event.into(),
                                })
                                .await;
                            if let Ok(r) = result {
                                let _ = host.consume_hook(&r, false);
                                if let HookPointOutcome::FileChanged(o) = r.outcome {
                                    for p in o.watch_paths {
                                        let _ = host.watch_path(PathBuf::from(p));
                                    }
                                }
                            }
                            let _ = host.post_event(RuntimeEvent {
                                id: uuid::Uuid::new_v4().to_string(),
                                context: Some(format!("File {event}: {}", path.display())),
                                notice: None,
                                wake: false,
                            });
                        }
                    }
                }
            }
        }));
        Ok(())
    }
    pub async fn stop_watching(&self) {
        self.stop.cancel();
        let task = self.watcher.lock().unwrap().take();
        if let Some(t) = task {
            t.abort();
            let _ = t.await;
        }
    }
}
impl Drop for Workspace {
    fn drop(&mut self) {
        self.stop.cancel();
        if let Some(t) = self.watcher.get_mut().unwrap().take() {
            t.abort();
        }
    }
}
// Do not follow directory symlinks: a watched tree must not recursively escape its root.
fn file_snapshot(root: &Path) -> Result<HashMap<PathBuf, Vec<u8>>, YourAiError> {
    let mut files = HashMap::new();
    let mut pending = vec![root.to_path_buf()];
    while let Some(path) = pending.pop() {
        let meta = match std::fs::symlink_metadata(&path) {
            Ok(meta) => meta,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
            Err(e) => return Err(error("watcher", e)),
        };
        if meta.is_dir() {
            for entry in std::fs::read_dir(&path).map_err(|e| error("watcher", e))? {
                pending.push(entry.map_err(|e| error("watcher", e))?.path());
            }
        } else if meta.is_file() {
            files.insert(
                path.clone(),
                std::fs::read(path).map_err(|e| error("watcher", e))?,
            );
        }
    }
    Ok(files)
}
fn resolve(cwd: &Path, path: &Path) -> Result<PathBuf, YourAiError> {
    let p = if path.is_absolute() {
        path.to_path_buf()
    } else {
        cwd.join(path)
    };
    p.canonicalize().map_err(|e| error("workspace", e))
}
async fn git(cwd: &Path, args: &[&str]) -> Result<(), YourAiError> {
    let mut c = tokio::process::Command::new("git");
    c.current_dir(cwd).args(args).kill_on_drop(true);
    let out = tokio::time::timeout(Duration::from_secs(30), c.output())
        .await
        .map_err(|_| error("git", "timeout"))?
        .map_err(|e| error("git", e))?;
    if !out.status.success() {
        return Err(error("git", String::from_utf8_lossy(&out.stderr)));
    }
    Ok(())
}
