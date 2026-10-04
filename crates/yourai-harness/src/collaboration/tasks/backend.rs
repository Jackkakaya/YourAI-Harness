//! Business mutations contain no hook protocol.
use super::*;

impl TaskBoard {
    fn run_create(&self, task: Task) -> Result<Task, YourAiError> {
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.insert(task.id.clone(), task.clone());
        atomic_write(&self.dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(task)
    }
    fn run_complete(&self, id: &str) -> Result<(), YourAiError> {
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.get_mut(id)
            .ok_or_else(|| error("tasks", "unknown task"))?
            .completed = true;
        atomic_write(&self.dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}

/// 实现侧业务接口：纯草稿生成与落盘，hook 生命周期在 core 公共模板。
impl TaskBoardProvider for TaskBoard {
    fn prepare_create<'a>(
        &'a self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> BoxFuture<'a, Result<TaskCreatePlan<'a>, YourAiError>> {
        Box::pin(async move {
            let guard = self.gate.lock().await;
            let task = Task {
                id: uuid::Uuid::new_v4().to_string(),
                subject,
                description,
                owner,
                completed: false,
                seq: self
                    .seq
                    .try_update(Ordering::Relaxed, Ordering::Relaxed, |n| n.checked_add(1))
                    .map_err(|_| error("tasks", "task creation sequence exhausted"))?
                    + 1,
            };
            Ok(TaskCreatePlan::new(task, guard))
        })
    }
    fn commit_create<'a>(
        &'a self,
        plan: TaskCreatePlan<'a>,
    ) -> BoxFuture<'a, Result<Task, YourAiError>> {
        Box::pin(async move { self.run_create(plan.task) })
    }
    fn prepare_complete<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskCompletePlan<'a>>, YourAiError>> {
        Box::pin(async move {
            let guard = self.gate.lock().await;
            let task = self
                .tasks
                .lock()
                .unwrap()
                .get(id)
                .cloned()
                .ok_or_else(|| error("tasks", "unknown task"))?;
            if task.completed {
                return Ok(None);
            }
            Ok(Some(TaskCompletePlan::new(task, guard)))
        })
    }
    fn commit_complete<'a>(
        &'a self,
        plan: TaskCompletePlan<'a>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move { self.run_complete(&plan.task.id) })
    }
    fn list(&self) -> Vec<Task> {
        TaskBoard::list(self)
    }
}
