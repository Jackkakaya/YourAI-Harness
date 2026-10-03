//! Optional modules own their effects and invoke hooks at actual lifecycle boundaries.
//!
//! `Workspace` intentionally holds a `Weak<SessionHost>` back-reference: every
//! mutation is a host operation (hook dispatch + journal gate + watch-set
//! update), so the dependency is real, not incidental.
mod backend;
mod operations;
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
        host.configure_input(self.skill_ids, self.memory_search_limit);
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

type WatchSnapshots = HashMap<PathBuf, HashMap<PathBuf, (u64, std::time::SystemTime)>>;

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
    pub fn config(&self) -> Result<Value, YourAiError> {
        let p = self.host()?.dir.join("config.json");
        if p.exists() {
            read_json(&p)
        } else {
            Ok(json!({}))
        }
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
                    let snapshot_root = root.clone();
                    let current =
                        match tokio::task::spawn_blocking(move || file_snapshot(&snapshot_root))
                            .await
                            .unwrap_or_else(|e| Err(error("watcher", e)))
                        {
                            Ok(snapshot) => snapshot,
                            Err(e) => {
                                let _ = host
                                    .post_event_async(RuntimeEvent {
                                        id: uuid::Uuid::new_v4().to_string(),
                                        context: None,
                                        notice: Some(format!("File watcher: {e}")),
                                        wake: false,
                                    })
                                    .await;
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
                            ws.file_changed(&host, &path, event).await;
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
// Change detection uses metadata only: rereading watched file contents every
// tick would multiply IO and memory by tree size.
fn file_snapshot(
    root: &Path,
) -> Result<HashMap<PathBuf, (u64, std::time::SystemTime)>, YourAiError> {
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
            let modified = meta.modified().map_err(|e| error("watcher", e))?;
            files.insert(path, (meta.len(), modified));
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
