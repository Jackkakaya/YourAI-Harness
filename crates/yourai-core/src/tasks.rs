//! TaskBoard 插件接口与固定公共任务操作。
//!
//! 业务实现（run 侧）只写草稿生成与落盘——[`TaskBoardProvider`]；
//! TaskCreated / TaskCompleted / TeammateIdle 的 hook 生命周期由本模块的
//! 公共模板持有（业务准备 → hook → 业务提交），对所有实现固定。
//! 已完成的任务重复 complete 走快路径，不触发事件。

use crate::error::{ErrorKind, YourAiError};
use crate::future::BoxFuture;
use crate::hooks::{HookEvent, HookHost};
use serde::{Deserialize, Serialize};

/// 一个任务条目。创建序单调递增；早于该字段存在的文件加载为 0，
/// 排序时靠前并以 id 稳定。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Task {
    pub id: String,
    pub subject: String,
    pub description: Option<String>,
    pub owner: Option<String>,
    pub completed: bool,
    #[serde(default)]
    pub seq: u64,
}

/// 创建计划：业务草稿 + 跨 hook 阶段持有的转换锁。
pub struct TaskCreatePlan<'a> {
    pub task: Task,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}
impl<'a> TaskCreatePlan<'a> {
    pub fn new(task: Task, guard: tokio::sync::MutexGuard<'a, ()>) -> Self {
        Self {
            task,
            _guard: guard,
        }
    }
}

/// 完成计划：待完成任务 + 跨 hook 阶段持有的转换锁。
pub struct TaskCompletePlan<'a> {
    pub task: Task,
    _guard: tokio::sync::MutexGuard<'a, ()>,
}
impl<'a> TaskCompletePlan<'a> {
    pub fn new(task: Task, guard: tokio::sync::MutexGuard<'a, ()>) -> Self {
        Self {
            task,
            _guard: guard,
        }
    }
}

/// 实现侧业务接口（run 侧）：纯任务板业务，不感知 hook。
///
/// - `prepare_*` 生成草稿/查找任务，不产生持久变更；
///   转换锁由返回的 plan 持有，跨过 hook 阶段直到 commit。
/// - `commit_*` 落盘并登记。
pub trait TaskBoardProvider: Send + Sync {
    /// 生成新任务草稿（id、创建序）；不落盘。
    fn prepare_create<'a>(
        &'a self,
        subject: String,
        description: Option<String>,
        owner: Option<String>,
    ) -> BoxFuture<'a, Result<TaskCreatePlan<'a>, YourAiError>>;
    /// 提交创建。
    fn commit_create<'a>(
        &'a self,
        plan: TaskCreatePlan<'a>,
    ) -> BoxFuture<'a, Result<Task, YourAiError>>;
    /// 准备完成：查找任务；已完成返回 `None`（快路径，不触发事件）。
    fn prepare_complete<'a>(
        &'a self,
        id: &'a str,
    ) -> BoxFuture<'a, Result<Option<TaskCompletePlan<'a>>, YourAiError>>;
    /// 提交完成。
    fn commit_complete<'a>(
        &'a self,
        plan: TaskCompletePlan<'a>,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    /// 全量任务（创建序排序）。
    fn list(&self) -> Vec<Task>;
}

fn task_error(message: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Provider {
        name: "tasks",
        message: message.to_string(),
    }
    .into()
}

/// 固定公共操作：创建任务。准备 → TaskCreated（阻断）→ 提交。
pub fn create_task<'a>(
    board: &'a dyn TaskBoardProvider,
    host: &'a dyn HookHost,
    team: &'a str,
    subject: String,
    description: Option<String>,
    owner: Option<String>,
) -> BoxFuture<'a, Result<Task, YourAiError>> {
    Box::pin(async move {
        let plan = board.prepare_create(subject, description, owner).await?;
        let task = plan.task.clone();
        let result = host
            .dispatch_hook(HookEvent::TaskCreated {
                task_id: task.id.clone(),
                task_subject: task.subject.clone(),
                task_description: task.description.clone(),
                teammate_name: task.owner.clone(),
                team_name: Some(team.to_owned()),
            })
            .await?;
        host.consume_hook_result(&result, true).await?;
        board.commit_create(plan).await
    })
}

/// 固定公共操作：完成任务。准备 → TaskCompleted（阻断，已完成不触发）→ 提交。
pub fn complete_task<'a>(
    board: &'a dyn TaskBoardProvider,
    host: &'a dyn HookHost,
    team: &'a str,
    id: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    Box::pin(async move {
        let Some(plan) = board.prepare_complete(id).await? else {
            return Ok(());
        };
        let task = plan.task.clone();
        let result = host
            .dispatch_hook(HookEvent::TaskCompleted {
                task_id: task.id,
                task_subject: task.subject,
                task_description: task.description,
                teammate_name: task.owner,
                team_name: Some(team.to_owned()),
            })
            .await?;
        host.consume_hook_result(&result, true).await?;
        board.commit_complete(plan).await
    })
}

/// 固定公共操作：队友空闲确认。存在未完成任务时直接失败；
/// 否则 TeammateIdle（阻断）。
pub fn teammate_idle<'a>(
    board: &'a dyn TaskBoardProvider,
    host: &'a dyn HookHost,
    team: &'a str,
    teammate: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    Box::pin(async move {
        if board
            .list()
            .iter()
            .any(|t| t.owner.as_deref() == Some(teammate) && !t.completed)
        {
            return Err(task_error("teammate has unfinished tasks"));
        }
        let result = host
            .dispatch_hook(HookEvent::TeammateIdle {
                teammate_name: teammate.to_owned(),
                team_name: team.to_owned(),
            })
            .await?;
        host.consume_hook_result(&result, true).await
    })
}
