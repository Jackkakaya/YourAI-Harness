//! 工具：定义侧（给模型的 schema）与执行侧（ToolContext 赋能）分离。
//!
//! 执行签名带 [`ToolContext`]——工具能发进度、能被取消（决策 5.5）。
//! 这是 subagent / browser / 长任务可显示的先决条件；
//! 工具仍然不知道 UI 存在——只对 outbox 讲协议。

use crate::chat::Tool;
use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::ui::OutSink;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

/// 工具执行期的能力注入（由 loop 在执行时装配）。
pub struct ToolContext<'a> {
    /// 发 Out（约定：工具只发 `Out::ToolProgress`；subagent 转发子事件也走这里）
    pub emit: &'a dyn OutSink,
    /// ESC 打断长工具
    pub cancel: &'a CancellationToken,
}

pub trait ToolHandler: Send + Sync {
    /// 工具名（模型调用时使用）
    fn name(&self) -> &str;

    /// 给模型看的 schema（genai::chat::Tool）
    fn definition(&self) -> Tool;

    /// 执行。返回 Err 时由 loop 转成 is_error 结果喂回模型，不逃逸成 turn 失败。
    fn execute(&self, tc: ToolContext<'_>, input: Value) -> BoxFuture<'_, Result<Value, YourAiError>>;
}

pub trait ToolRegistry: Send + Sync {
    fn register(&self, handler: std::sync::Arc<dyn ToolHandler>);
    /// 批量注册（MCP connect → Vec 一批进）
    fn extend(&self, handlers: Vec<std::sync::Arc<dyn ToolHandler>>) {
        for h in handlers {
            self.register(h);
        }
    }
    fn unregister(&self, name: &str);
    fn has(&self, name: &str) -> bool;
    /// 所有工具的 schema（发给模型）
    fn definitions(&self) -> Vec<Tool>;
    fn execute(
        &self,
        tc: ToolContext<'_>,
        name: &str,
        input: Value,
    ) -> BoxFuture<'_, Result<Value, YourAiError>>;
    fn count(&self) -> usize;
}
