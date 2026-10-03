//! 输入接纳的公共入口（固定缝）。
//!
//! UserPromptSubmit 的生命周期（可阻断、附加上下文在提交后落盘）由缝实现
//! 持有；接纳状态机（排队、附件解析、记忆召回、技能注入）是运行时业务。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::protocol::In;

/// Infrastructure bridge between the Core entry and a runtime's execution.
#[doc(hidden)]
pub trait InputOperation: Send {
    /// 返回 `true` 表示输入被接纳并已提交历史；`false` 表示被显式拒绝。
    fn accept_bound<'a>(&'a mut self, input: In) -> BoxFuture<'a, Result<bool, YourAiError>>;
}

/// 固定公共入口：接纳一条用户输入（UserPromptSubmit 生命周期在框架操作内）。
pub fn accept<'a>(
    operation: &'a mut dyn InputOperation,
    input: In,
) -> BoxFuture<'a, Result<bool, YourAiError>> {
    operation.accept_bound(input)
}
