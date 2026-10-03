use std::{sync::Arc, time::Duration};

pub use crate::execution::AttachmentImageConfig;
/// Policy defaults, not additional Providers. TurnLimits can impose stricter limits.
#[derive(Debug, Clone)]
pub struct LoopConfig {
    /// Default scheduling iteration cap.
    pub steps: Option<u32>,
    /// Managed output storage; assembled harnesses share their catalog store.
    pub tool_output: Option<Arc<crate::tools::ToolOutputStore>>,
    /// Explicitly selected skills; listing a skill does not activate it.
    pub skill_ids: Vec<String>,
    pub memory_search_limit: usize,
    pub memory_max_chars: usize,
    /// OpenCode-compatible attachment image normalization policy.
    pub attachment_image: AttachmentImageConfig,
    /// Char cap for text files attached by reference (before line-window
    /// selection the cap applies to the selected window). Directories and
    /// binary media are unaffected.
    pub attachment_text_max_chars: usize,
    pub max_model_retries: u32,
    pub max_overflow_compactions: u32,
    pub max_stop_continuations: u32,
    pub max_permission_rechecks: u32,
    pub retry_delay: Duration,
    pub retry_max_delay: Duration,
    /// Optional bound for generic provider operations; no implicit turn deadline.
    pub operation_timeout: Option<Duration>,
    /// Optional host tool bound. Built-in tools own their default timeouts.
    pub tool_timeout: Option<Duration>,
    pub approval_timeout: Option<Duration>,
    pub hook_timeout: Option<Duration>,
    /// Optional bound for durable cleanup, independent of the cancelled turn.
    pub cleanup_timeout: Option<Duration>,
    /// Grace for tools to settle after cancellation, shared with failure reporting.
    pub tool_cleanup_timeout: Duration,
}
impl Default for LoopConfig {
    fn default() -> Self {
        Self {
            steps: None,
            tool_output: None,
            skill_ids: vec![],
            memory_search_limit: 0,
            memory_max_chars: 8000,
            attachment_image: AttachmentImageConfig::default(),
            attachment_text_max_chars: 50_000,
            max_model_retries: 5,
            max_overflow_compactions: 1,
            max_stop_continuations: 3,
            max_permission_rechecks: 1,
            retry_delay: Duration::from_secs(2),
            retry_max_delay: Duration::from_secs(30),
            operation_timeout: None,
            tool_timeout: None,
            approval_timeout: None,
            hook_timeout: None,
            cleanup_timeout: None,
            tool_cleanup_timeout: Duration::from_millis(250),
        }
    }
}

impl From<&LoopConfig> for crate::execution::ExecutionConfig {
    fn from(c: &LoopConfig) -> Self {
        Self {
            tool_output: c.tool_output.clone(),
            skill_ids: c.skill_ids.clone(),
            memory_search_limit: c.memory_search_limit,
            memory_max_chars: c.memory_max_chars,
            attachment_image: c.attachment_image.clone(),
            attachment_text_max_chars: c.attachment_text_max_chars,
            max_model_retries: c.max_model_retries,
            max_overflow_compactions: c.max_overflow_compactions,
            max_stop_continuations: c.max_stop_continuations,
            max_permission_rechecks: c.max_permission_rechecks,
            retry_delay: c.retry_delay,
            retry_max_delay: c.retry_max_delay,
            operation_timeout: c.operation_timeout,
            tool_timeout: c.tool_timeout,
            approval_timeout: c.approval_timeout,
            hook_timeout: c.hook_timeout,
            cleanup_timeout: c.cleanup_timeout,
            tool_cleanup_timeout: c.tool_cleanup_timeout,
        }
    }
}
