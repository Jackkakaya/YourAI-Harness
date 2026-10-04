pub use crate::execution::AttachmentImageConfig;
use crate::execution::ExecutionConfig;

/// Scheduling policy and its independently owned execution configuration.
#[derive(Debug, Clone, Default)]
pub struct LoopConfig {
    pub steps: Option<u32>,
    pub execution: ExecutionConfig,
}
