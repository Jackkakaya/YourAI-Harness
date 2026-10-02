//! Business mutations contain no hook protocol.
use super::*;

impl TaskBoard {
    pub(super) fn run_create(
        &self,
        dir: &std::path::Path,
        task: Task,
    ) -> Result<Task, YourAiError> {
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.insert(task.id.clone(), task.clone());
        atomic_write(&dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(task)
    }
    pub(super) fn run_complete(&self, dir: &std::path::Path, id: &str) -> Result<(), YourAiError> {
        let mut tasks = self.tasks.lock().unwrap();
        let mut next = tasks.clone();
        next.get_mut(id)
            .ok_or_else(|| error("tasks", "unknown task"))?
            .completed = true;
        atomic_write(&dir.join("tasks.json"), &next)?;
        *tasks = next;
        self.version.fetch_add(1, Ordering::Relaxed);
        Ok(())
    }
}
