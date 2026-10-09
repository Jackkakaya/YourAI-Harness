//! One task manager owns transitions, tool input and the session's persisted task view.
use crate::{
    error,
    file_store::{atomic_write, read_json},
    prelude::*,
    runtime::SessionHost,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex, Weak,
    },
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub completed: bool,
    #[serde(default)]
    pub seq: u64,
}

pub struct TaskManager {
    host: Weak<SessionHost>,
    sessions: Arc<dyn SessionManager>,
    tasks: Mutex<HashMap<String, Task>>,
    gate: Arc<tokio::sync::Mutex<()>>,
    version: AtomicU64,
    pub(crate) team: String,
}
impl TaskManager {
    /// The host owns initialization and publishes only one manager per session.
    pub(crate) fn open(host: &Arc<SessionHost>, team: String) -> Result<Arc<Self>, YourAiError> {
        let sessions = host.agent().ctx().session()?;
        let id = host.context().id;
        let mut tasks: HashMap<_, _> = sessions
            .read_tasks(&id)?
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        // Retain the old file as a backup, but retire it after a confirmed import.
        // Saving first makes a failed marker write retryable without overwriting SQLite.
        let legacy = host.directory().join("tasks.json");
        let migrated = host.directory().join("tasks.migrated.json");
        if !migrated.is_file() && legacy.exists() {
            let old: HashMap<String, Task> = read_json(&legacy)?;
            if old.iter().any(|(key, task)| key != &task.id) {
                return Err(error("tasks", "legacy task key differs from task identity"));
            }
            let missing: Vec<_> = old
                .into_values()
                .filter(|task| !tasks.contains_key(&task.id))
                .collect();
            if !missing.is_empty() {
                for task in sessions.save_tasks(&id, missing)? {
                    tasks.insert(task.id.clone(), task);
                }
            }
            atomic_write(&migrated, &true)?;
        }
        Ok(Arc::new(Self {
            host: Arc::downgrade(host),
            sessions,
            tasks: Mutex::new(tasks),
            gate: Arc::new(tokio::sync::Mutex::new(())),
            version: AtomicU64::new(0),
            team,
        }))
    }

    fn host(&self) -> Result<Arc<SessionHost>, YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        host.ensure_open()?;
        Ok(host)
    }
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }
    pub fn list(&self) -> Vec<Task> {
        let mut tasks: Vec<_> = self.tasks.lock().unwrap().values().cloned().collect();
        tasks.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));
        tasks
    }
    pub async fn create(
        &self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> Result<Task, YourAiError> {
        let guard = self.gate.clone().lock_owned().await;
        let host = self.host()?;
        let activity = host.activity()?;
        let seq = self
            .tasks
            .lock()
            .unwrap()
            .values()
            .map(|task| task.seq)
            .max()
            .unwrap_or(0)
            .checked_add(1)
            .ok_or_else(|| error("tasks", "task creation sequence exhausted"))?;
        let task = Task {
            id: uuid::Uuid::new_v4().to_string(),
            subject,
            description,
            owner,
            completed: false,
            seq,
        };
        let result = host
            .dispatch_active(HookEvent::TaskCreated {
                task_id: task.id.clone(),
                task_subject: task.subject.clone(),
                task_description: task.description.clone(),
                teammate_name: task.owner.clone(),
                team_name: Some(self.team.clone()),
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        self.save(&host, task, guard, activity).await
    }
    pub async fn complete(&self, id: &str) -> Result<(), YourAiError> {
        let guard = self.gate.clone().lock_owned().await;
        let host = self.host()?;
        let activity = host.activity()?;
        let mut task = self
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
            .dispatch_active(HookEvent::TaskCompleted {
                task_id: task.id.clone(),
                task_subject: task.subject.clone(),
                task_description: task.description.clone(),
                teammate_name: task.owner.clone(),
                team_name: Some(self.team.clone()),
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await?;
        task.completed = true;
        self.save(&host, task, guard, activity).await?;
        Ok(())
    }
    pub async fn idle(&self, teammate: &str) -> Result<(), YourAiError> {
        let _guard = self.gate.lock().await;
        let host = self.host()?;
        let _activity = host.activity()?;
        if self
            .tasks
            .lock()
            .unwrap()
            .values()
            .any(|task| task.owner.as_deref() == Some(teammate) && !task.completed)
        {
            return Err(error("tasks", "teammate has unfinished tasks"));
        }
        let result = host
            .dispatch_active(HookEvent::TeammateIdle {
                teammate_name: teammate.into(),
                team_name: self.team.clone(),
            })
            .await?;
        result.ensure_allowed()?;
        host.apply_hook(&result).await
    }

    async fn save(
        &self,
        host: &Arc<SessionHost>,
        task: Task,
        guard: tokio::sync::OwnedMutexGuard<()>,
        activity: crate::runtime::Activity,
    ) -> Result<Task, YourAiError> {
        let manager = host
            .tasks
            .get()
            .cloned()
            .ok_or_else(|| error("tasks", "manager not attached"))?;
        host.blocking(move |host| {
            // An owned worker finishes both the durable write and cache publication if
            // its waiter leaves. Keep the transition lock until that acknowledgement.
            let _guard = guard;
            let _activity = activity;
            let _write = host.resource_lock()?;
            let saved = manager
                .sessions
                .save_tasks(&host.context().id, vec![task])?
                .pop()
                .ok_or_else(|| error("tasks", "session manager returned no saved task"))?;
            manager
                .tasks
                .lock()
                .unwrap()
                .insert(saved.id.clone(), saved.clone());
            manager.version.fetch_add(1, Ordering::Release);
            Ok(saved)
        })
        .await?
    }
}

impl ToolProvider for TaskManager {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("tasks")
            .with_description("Track coding TODOs. Create a task with a subject, list tasks, and mark a task complete by its id only when the work is done.")
            .with_schema(json!({
                "type":"object",
                "properties":{
                    "action":{"enum":["list","create","complete","idle"]},
                    "subject":{"type":"string"},
                    "id":{"type":"string"},
                    "owner":{"type":"string"}
                },
                "required":["action"]
            }))
    }
    fn security_context(&self, input: &Value, _cwd: Option<&Path>) -> SecurityContext {
        SecurityContext {
            action: "tasks".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            if tc.cancel.is_cancelled() {
                return Err(AbortReason::Cancelled.into());
            }
            match input["action"].as_str() {
                Some("list") => Ok(json!(self.list())),
                Some("create") => Ok(json!(
                    self.create(
                        input["subject"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "subject required"))?
                            .into(),
                        None,
                        input["owner"].as_str().map(str::to_owned),
                    )
                    .await?
                )),
                Some("complete") => {
                    self.complete(
                        input["id"]
                            .as_str()
                            .ok_or_else(|| error("tasks", "id required"))?,
                    )
                    .await?;
                    Ok(json!({"completed":true}))
                }
                Some("idle") => {
                    self.idle(
                        input["owner"]
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
