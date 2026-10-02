//! 工作区操作的公共入口（固定缝）。
//!
//! 宿主（SessionHost）拥有 journal gate、监视集与运行时事件队列——
//! 工作区变更本质上是宿主操作，业务与状态强耦合在宿主侧。
//! 本缝固定入口符号：Setup / Notification / ConfigChange / InstructionsLoaded /
//! WorktreeCreate / WorktreeRemove / CwdChanged / FileChanged 的生命周期
//! 由缝实现持有；调用方经由 core 入口获得契约。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use serde_json::Value;
use std::path::{Path, PathBuf};

/// Infrastructure bridge between the Core entries and a runtime's execution.
#[doc(hidden)]
pub trait WorkspaceOperation: Send + Sync {
    fn setup_bound<'a>(&'a self, trigger: &'a str) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn load_instructions_bound<'a>(
        &'a self,
        path: &'a Path,
        reason: &'a str,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn notify_bound<'a>(
        &'a self,
        message: &'a str,
        kind: &'a str,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn change_config_bound<'a>(
        &'a self,
        source: &'a str,
        value: Value,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn change_cwd_bound<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn create_worktree_bound<'a>(
        &'a self,
        name: &'a str,
    ) -> BoxFuture<'a, Result<PathBuf, YourAiError>>;
    fn remove_worktree_bound<'a>(&'a self, name: &'a str)
        -> BoxFuture<'a, Result<(), YourAiError>>;
    fn file_changed_bound<'a>(&'a self, path: &'a Path, event: &'a str) -> BoxFuture<'a, ()>;
}

/// 固定公共入口：初始化/维护（Setup）。
pub fn setup<'a>(
    operation: &'a dyn WorkspaceOperation,
    trigger: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.setup_bound(trigger)
}

/// 固定公共入口：加载指令文件（InstructionsLoaded）。
pub fn load_instructions<'a>(
    operation: &'a dyn WorkspaceOperation,
    path: &'a Path,
    reason: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.load_instructions_bound(path, reason)
}

/// 固定公共入口：发送通知（Notification）。
pub fn notify<'a>(
    operation: &'a dyn WorkspaceOperation,
    message: &'a str,
    kind: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.notify_bound(message, kind)
}

/// 固定公共入口：变更运行时配置（ConfigChange）。
pub fn change_config<'a>(
    operation: &'a dyn WorkspaceOperation,
    source: &'a str,
    value: Value,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.change_config_bound(source, value)
}

/// 固定公共入口：切换工作目录（CwdChanged）。
pub fn change_cwd<'a>(
    operation: &'a dyn WorkspaceOperation,
    path: &'a Path,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.change_cwd_bound(path)
}

/// 固定公共入口：创建 worktree（WorktreeCreate，hook 可提供路径）。
pub fn create_worktree<'a>(
    operation: &'a dyn WorkspaceOperation,
    name: &'a str,
) -> BoxFuture<'a, Result<PathBuf, YourAiError>> {
    operation.create_worktree_bound(name)
}

/// 固定公共入口：移除 worktree（WorktreeRemove）。
pub fn remove_worktree<'a>(
    operation: &'a dyn WorkspaceOperation,
    name: &'a str,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    operation.remove_worktree_bound(name)
}

/// 固定公共入口：文件变化回调（FileChanged）。错误被吞掉——监视路径是尽力而为。
pub fn file_changed<'a>(
    operation: &'a dyn WorkspaceOperation,
    path: &'a Path,
    event: &'a str,
) -> BoxFuture<'a, ()> {
    operation.file_changed_bound(path, event)
}
