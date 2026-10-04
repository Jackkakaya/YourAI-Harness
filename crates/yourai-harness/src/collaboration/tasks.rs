mod backend;
mod operations;
use crate::{
    error,
    storage::{atomic_write, read_json},
    SessionHost,
};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
};
use yourai_core::prelude::*;

/// Task 条目与固定公共任务操作住在 core；此处仅保留业务实现。
pub use yourai_core::tasks::{Task, TaskBoardProvider};

pub struct TaskBoard {
    host: Weak<SessionHost>,
    dir: PathBuf,
    tasks: Mutex<HashMap<String, Task>>,
    gate: tokio::sync::Mutex<()>,
    seq: AtomicU64,
    /// Bumped on every committed mutation so pollers can skip unchanged
    /// copies of the whole board.
    version: AtomicU64,
    pub team: String,
}
impl TaskBoard {
    pub fn new(host: &Arc<SessionHost>, team: impl Into<String>) -> Result<Arc<Self>, YourAiError> {
        let path = host.dir.join("tasks.json");
        let tasks = if path.exists() {
            read_json(&path)?
        } else {
            HashMap::new()
        };
        // Continue the sequence after the highest persisted task so a reload
        // never reuses an order slot.
        let seq = tasks.values().map(|t: &Task| t.seq).max().unwrap_or(0);
        Ok(Arc::new(Self {
            host: Arc::downgrade(host),
            dir: host.dir.clone(),
            tasks: Mutex::new(tasks),
            gate: tokio::sync::Mutex::new(()),
            seq: AtomicU64::new(seq),
            version: AtomicU64::new(0),
            team: team.into(),
        }))
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }
    pub fn list(&self) -> Vec<Task> {
        let mut v: Vec<_> = self.tasks.lock().unwrap().values().cloned().collect();
        // Creation order, not UUID order: a todo board read top-to-bottom
        // should reflect the order work was planned in.
        v.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));
        v
    }
}
impl ToolHandler for TaskBoard {
    fn name(&self) -> &str {
        "tasks"
    }
    fn definition(&self) -> Tool {
        Tool::new("tasks").with_description("Track coding TODOs. Create a task with a subject, list tasks, and mark a task complete by its id only when the work is done.").with_schema(json!({"type":"object","properties":{"action":{"enum":["list","create","complete","idle"]},"subject":{"type":"string"},"id":{"type":"string"},"owner":{"type":"string"}},"required":["action"]}))
    }
    fn security_context(&self, v: &Value) -> SecurityContext {
        SecurityContext {
            action: "tasks".into(),
            input: v.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn run<'a>(
        &'a self,
        _: ToolContext<'a>,
        v: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            match v["action"].as_str() {
                Some("list") => Ok(json!(self.list())),
                Some("create") => Ok(json!(
                    self.create(
                        v["subject"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "subject required"))?
                            .into(),
                        None,
                        v["owner"].as_str().map(str::to_owned)
                    )
                    .await?
                )),
                Some("complete") => {
                    self.complete(
                        v["id"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "id required"))?,
                    )
                    .await?;
                    Ok(json!({"completed":true}))
                }
                Some("idle") => {
                    self.idle(
                        v["owner"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "owner required"))?,
                    )
                    .await?;
                    Ok(json!({"idle":true}))
                }
                _ => Err(error("tasks", "unknown action")),
            }
        })
    }
}
