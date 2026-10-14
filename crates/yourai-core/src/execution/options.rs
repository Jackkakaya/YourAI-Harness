use crate::{compactor::ContextPolicy, turn::TurnLimits};
use std::{path::PathBuf, time::Duration};

crate::settings_fields! {
/// Image attachment policy, mirroring opencode `attachment.image.*`
/// (`packages/opencode/src/image/image.ts`). Over-sized images are resized
/// (Lanczos3, PNG then JPEG at descending qualities) instead of rejected;
/// only an image that cannot be brought within limits fails.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ImagePolicy {
    auto_resize: bool => set_auto_resize [value],
    max_width: u32 => set_max_width [value],
    max_height: u32 => set_max_height [value],
    /// Ceiling on the base64 payload length, matching the provider-side
    /// limit the number applies to (opencode: 5 MiB base64).
    max_base64_bytes: usize => set_max_base64_bytes [value],
}
}
impl Default for ImagePolicy {
    fn default() -> Self {
        Self {
            auto_resize: true,
            max_width: 2000,
            max_height: 2000,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }
}

crate::settings_fields! {
/// Plain execution data copied from the application config for one turn.
/// Standalone callers may supply these values directly. This type owns no storage.
/// TurnLimits can impose stricter limits without changing the shared defaults.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ExecutionOptions {
    skill_ids: Vec<String> => set_skill_ids [ref],
    memory_search_limit: usize => set_memory_search_limit [value],
    #[serde(rename = "context")]
    context_policy: ContextPolicy => set_context_policy [mut context_policy_mut],
    #[serde(alias = "max_model_calls")]
    steps: Option<u32> => set_steps [value],
    model_timeout: Option<Duration> => set_model_timeout [value],
    allow_background_wake: bool => set_allow_background_wake [value],
    max_followups: usize => set_max_followups [value],
    instruction_paths: Vec<PathBuf> => set_instruction_paths [ref],
    workspace_enabled: bool => set_workspace_enabled [value],
    memory_max_chars: usize => set_memory_max_chars [value],
    /// OpenCode-compatible attachment image normalization policy.
    attachment_image: ImagePolicy => set_attachment_image [mut attachment_image_mut],
    /// Char cap for text files attached by reference (before line-window
    /// selection the cap applies to the selected window). Directories and
    /// binary media are unaffected.
    attachment_text_max_chars: usize => set_attachment_text_max_chars [value],
    max_model_retries: u32 => set_max_model_retries [value],
    max_overflow_compactions: u32 => set_max_overflow_compactions [value],
    max_stop_continuations: u32 => set_max_stop_continuations [value],
    max_permission_rechecks: u32 => set_max_permission_rechecks [value],
    retry_delay: Duration => set_retry_delay [value],
    retry_max_delay: Duration => set_retry_max_delay [value],
    /// Optional bound for generic provider operations; no implicit turn deadline.
    operation_timeout: Option<Duration> => set_operation_timeout [value],
    /// Optional host tool bound. Built-in tools own their default timeouts.
    tool_timeout: Option<Duration> => set_tool_timeout [value],
    approval_timeout: Option<Duration> => set_approval_timeout [value],
    hook_timeout: Option<Duration> => set_hook_timeout [value],
    /// Optional bound for durable cleanup, independent of the cancelled turn.
    cleanup_timeout: Option<Duration> => set_cleanup_timeout [value],
    /// Grace for tools to settle after cancellation, shared with failure reporting.
    tool_cleanup_timeout: Duration => set_tool_cleanup_timeout [value],
}
}
impl Default for ExecutionOptions {
    fn default() -> Self {
        Self {
            skill_ids: vec![],
            memory_search_limit: 0,
            context_policy: ContextPolicy::default(),
            steps: None,
            model_timeout: None,
            allow_background_wake: true,
            max_followups: 64,
            instruction_paths: vec![],
            workspace_enabled: false,
            memory_max_chars: 8000,
            attachment_image: ImagePolicy::default(),
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

impl ExecutionOptions {
    /// Resolve per-turn limits once. Existing configured limits are hard ceilings.
    pub fn resolve(&self, limits: &TurnLimits) -> Self {
        let mut config = self.clone();
        fn tighter<T: Ord + Copy>(default: Option<T>, limit: Option<T>) -> Option<T> {
            match (default, limit) {
                (Some(a), Some(b)) => Some(a.min(b)),
                (a, b) => a.or(b),
            }
        }
        config.steps = tighter(config.steps, limits.steps);
        config.model_timeout = tighter(config.model_timeout, limits.model_timeout);
        config.tool_timeout = tighter(config.tool_timeout, limits.tool_timeout);
        config.approval_timeout = tighter(config.approval_timeout, limits.approval_timeout);
        config.hook_timeout = tighter(config.hook_timeout, limits.hook_timeout);
        config
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn per_turn_limits_can_only_tighten_configured_ceilings() {
        let config = {
            let mut settings: ExecutionOptions = Default::default();
            settings.set_steps(Some(5));
            settings.set_tool_timeout(Some(Duration::from_secs(10)));
            settings.set_hook_timeout(None);
            settings
        };
        let limits = TurnLimits {
            steps: Some(9),
            tool_timeout: Some(Duration::from_secs(2)),
            hook_timeout: Some(Duration::from_secs(3)),
            ..Default::default()
        };
        let resolved = config.resolve(&limits);
        assert_eq!(resolved.steps, Some(5));
        assert_eq!(resolved.tool_timeout, Some(Duration::from_secs(2)));
        assert_eq!(resolved.hook_timeout, Some(Duration::from_secs(3)));
        assert_eq!(config.tool_timeout, Some(Duration::from_secs(10)));
    }
}
