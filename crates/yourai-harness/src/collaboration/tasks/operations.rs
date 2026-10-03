//! Public transition wrappers own hooks; mutations are private business methods.
use super::*;

impl TaskBoard {
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
        host.consume_hook_async(&result, true).await?;
        self.run_create(&host.dir, task)
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
        host.consume_hook_async(&result, true).await?;
        self.run_complete(&host.dir, id)
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
        host.consume_hook_async(&result, true).await
    }
}
