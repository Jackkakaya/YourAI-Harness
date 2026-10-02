use std::{sync::Arc, time::Duration};

/// Image attachment policy, mirroring opencode `attachment.image.*`
/// (`packages/opencode/src/image/image.ts`). Over-sized images are resized
/// (Lanczos3, PNG then JPEG at descending qualities) instead of rejected;
/// only an image that cannot be brought within limits fails.
#[derive(Debug, Clone)]
pub struct AttachmentImageConfig {
    pub auto_resize: bool,
    pub max_width: u32,
    pub max_height: u32,
    /// Ceiling on the base64 payload length, matching the provider-side
    /// limit the number applies to (opencode: 5 MiB base64).
    pub max_base64_bytes: usize,
}
impl Default for AttachmentImageConfig {
    fn default() -> Self {
        Self {
            auto_resize: true,
            max_width: 2000,
            max_height: 2000,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }
}

/// Policy defaults, not additional Providers. TurnLimits can impose stricter limits.
#[derive(Debug, Clone)]
pub struct ExecutionConfig {
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
impl Default for ExecutionConfig {
    fn default() -> Self {
        Self {
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
