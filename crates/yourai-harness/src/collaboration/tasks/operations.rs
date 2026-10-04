//! Public transitions forward to the fixed core templates.
use super::*;

impl TaskBoard {
    pub async fn create(
        &self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> Result<Task, YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        // 固定公共入口：TaskCreated 生命周期在 core 模板。
        yourai_core::tasks::create_task(
            self,
            host.as_ref(),
            &self.team,
            subject,
            description,
            owner,
        )
        .await
    }
    pub async fn complete(&self, id: &str) -> Result<(), YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        // 固定公共入口：TaskCompleted 生命周期在 core 模板（已完成快路径不触发）。
        yourai_core::tasks::complete_task(self, host.as_ref(), &self.team, id).await
    }
    pub async fn idle(&self, teammate: &str) -> Result<(), YourAiError> {
        let host = self
            .host
            .upgrade()
            .ok_or_else(|| error("tasks", "host gone"))?;
        // 固定公共入口：TeammateIdle 生命周期在 core 模板。
        yourai_core::tasks::teammate_idle(self, host.as_ref(), &self.team, teammate).await
    }
}
