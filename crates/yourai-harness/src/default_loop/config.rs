pub use crate::execution::AttachmentImageConfig;
#[derive(Debug, Clone, Default)]
pub struct LoopConfig {
    pub steps: Option<u32>,
    pub execution: crate::execution::ExecutionConfig,
}
