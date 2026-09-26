use crate::{
    error,
    storage::{atomic_write, read_json},
    SessionHost,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
};
use yourai_core::prelude::*;
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub completed: bool,
    /// Monotonic creation order. Files written before this field existed
    /// load with 0; they sort first and keep their id order as tie-break.
    #[serde(default)]
    pub seq: u64,
}
pub struct TaskBoard {
    host: Weak<SessionHost>,
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
    pub async fn create(
        &self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> Result<Task, YourAiError> {
        let _gate = self.gate.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            subject,
            description,
            owner,
            completed: false,
            seq: self
                .seq
                .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                .map_err(|_| error("tasks", "task creation sequence exhausted"))?
                + 1,
        };
        let result = host
            .dispatch(HookEvent::TaskCreated {
                task_id: task.id.clone(),
                task_subject: task.subject.clone(),
                task_description: task.description.clone(),
                teammate_name: task.owner.clone(),
                team_name: Some(self.team.clone()),
            })
            .await?;
        host.consume_hook(&result, true)?;
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.insert(task.id.clone(), task.clone());
        atomic_write(&host.dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(task)
    }
    pub async fn complete(&self, id: &str) -> Result<(), YourAiError> {
        let _gate = self.gate.lock().await;
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        let task = self
            .tasks
            .lock()
            .unwrap()
            .get(id)
            .cloned()
            .ok_or_else(|| error("tasks", "unknown task"))?;
        if task.completed {
            return Ok(());
        }
        let result = host
            .dispatch(HookEvent::TaskCompleted {
                task_id: task.id,
                task_subject: task.subject,
                task_description: task.description,
                teammate_name: task.owner,
                team_name: Some(self.team.clone()),
            })
            .await?;
        host.consume_hook(&result, true)?;
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.get_mut(id).unwrap().completed = true;
        atomic_write(&host.dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
    pub async fn idle(&self, teammate: &str) -> Result<(), YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        if self
            .list()
            .iter()
            .any(|t| t.owner.as_deref() == Some(teammate) && !t.completed)
        {
            return Err(error("tasks", "teammate has unfinished tasks"));
        }
        let result = host
            .dispatch(HookEvent::TeammateIdle {
                teammate_name: teammate.into(),
                team_name: self.team.clone(),
            })
            .await?;
        host.consume_hook(&result, true)
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
    fn execute<'a>(
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
