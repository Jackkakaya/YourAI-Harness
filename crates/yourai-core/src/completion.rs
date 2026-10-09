//! Completion result for the fixed core Turn template.

/// 一次完成请求的结果：完成，或 hook 反馈要求继续工作。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Completion {
    Completed,
    NeedsMoreWork,
}
