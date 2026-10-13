//! One task manager owns transitions and the session's persisted task view.
use crate::{error, file_store::read_json, prelude::*, runtime::SessionManager};
use serde::{Deserialize, Serialize};
use std::{
    collections::HashMap,
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
    host: Weak<SessionManager>,
    sessions: Arc<dyn SessionStore>,
    tasks: Mutex<HashMap<String, Task>>,
    gate: Arc<tokio::sync::Mutex<()>>,
    version: AtomicU64,
    pub(crate) team: String,
}
impl TaskManager {
    /// The host owns initialization and publishes only one manager per session.
    pub(crate) fn open(host: &Arc<SessionManager>, team: String) -> Result<Arc<Self>, YourAiError> {
        let sessions = host.agent().session()?;
        let id = host.environment().id;
        let mut tasks: HashMap<_, _> = sessions
            .read_tasks(&id)?
            .into_iter()
            .map(|task| (task.id.clone(), task))
            .collect();
        // Retain the old file as a backup, but retire it after a confirmed import.
        // Saving first makes a failed marker write retryable without overwriting SQLite.
        let legacy = host.directory().join("tasks.json");
        if sessions.read_state(&id, "task_import")?.is_none() && legacy.exists() {
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
            sessions.save_state(&id, "task_import", None, serde_json::json!(true))?;
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

    fn host(&self) -> Result<Arc<SessionManager>, YourAiError> {
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
    pub fn snapshot_since(&self, previous: Option<u64>) -> Option<(u64, Vec<Task>)> {
        let current = self.tasks.lock().unwrap();
        let version = self.version.load(Ordering::Acquire);
        if previous == Some(version) {
            return None;
        }
        let mut tasks: Vec<_> = current.values().cloned().collect();
        tasks.sort_by(|a, b| a.seq.cmp(&b.seq).then_with(|| a.id.cmp(&b.id)));
        Some((version, tasks))
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
        host: &Arc<SessionManager>,
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
                .save_tasks(&host.environment().id, vec![task])?
                .pop()
                .ok_or_else(|| error("tasks", "session manager returned no saved task"))?;
            let mut tasks = manager.tasks.lock().unwrap();
            tasks.insert(saved.id.clone(), saved.clone());
            manager.version.fetch_add(1, Ordering::Release);
            Ok(saved)
        })
        .await?
    }
}
