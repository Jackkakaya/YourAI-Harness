//! Context maintenance policy and committed outcomes.
use crate::chat::{GenaiUsage, Tool};
use std::{
    sync::{atomic::AtomicU32, Arc},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionTrigger {
    Threshold,
    Overflow,
    Manual,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPolicy {
    /// Total context window. None reports an unknown budget.
    pub context_window: Option<u64>,
    /// Independent input limit, if the provider supplies one.
    pub input_limit: Option<u64>,
    pub output_reserve: u64,
    pub safety_margin: u64,
    pub advance_tokens: u64,
    pub keep_recent_tokens: u64,
    pub summary_tokens: u64,
    pub tool_output_chars: usize,
    pub prune_enabled: bool,
    pub prune_min_savings: u64,
    pub prune_growth: u64,
    pub summary_min_savings: u64,
}
impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            context_window: None,
            input_limit: None,
            output_reserve: 4096,
            safety_margin: 1024,
            advance_tokens: 4096,
            keep_recent_tokens: 20_000,
            summary_tokens: 2000,
            tool_output_chars: 16_000,
            prune_enabled: false,
            prune_min_savings: 1024,
            prune_growth: 2048,
            summary_min_savings: 256,
        }
    }
}
impl ContextPolicy {
    pub fn validate(&self) -> Result<(), crate::YourAiError> {
        if self.output_reserve == 0
            || self.output_reserve > u32::MAX as u64
            || self.summary_tokens == 0
            || self.tool_output_chars < 256
            || self.input_budget() == Some(0)
        {
            return Err(crate::ErrorKind::Config("invalid context policy: require positive output/summary budgets, tool_output_chars >= 256, and nonzero available input".into()).into());
        }
        Ok(())
    }
    pub fn input_budget(&self) -> Option<u64> {
        match (
            self.input_limit,
            self.context_window
                .map(|w| w.saturating_sub(self.output_reserve)),
        ) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|w| w.saturating_sub(self.safety_margin))
    }
}
#[derive(Debug, Clone)]
pub struct CompactionRequest {
    pub trigger: CompactionTrigger,
    pub custom_instructions: Option<String>,
    pub target_tokens: Option<u64>,
    pub deadline: Option<Instant>,
    pub tools: Vec<Tool>,
    /// Shared counter survives errors/cancellation so the caller charges attempted calls.
    pub calls: Arc<AtomicU32>,
    pub max_model_calls: u32,
    pub usage: Arc<std::sync::Mutex<Vec<GenaiUsage>>>,
}
impl CompactionRequest {
    pub fn new(trigger: CompactionTrigger) -> Self {
        Self {
            trigger,
            custom_instructions: None,
            target_tokens: None,
            deadline: None,
            tools: vec![],
            calls: Arc::new(AtomicU32::new(0)),
            max_model_calls: 8,
            usage: Arc::new(std::sync::Mutex::new(vec![])),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactAction {
    Unchanged,
    Pruned,
    Summarized,
}
#[derive(Debug, Clone)]
pub struct CompactionResult {
    pub action: CompactAction,
    pub tokens_before: u64,
    pub tokens_after: u64,
    pub reason: String,
    /// Informational usage only; already recorded by ContextManager.
    pub usage: Option<crate::protocol::Usage>,
    /// Post-commit hooks can stop continuation without undoing committed history.
    pub stop_reason: Option<String>,
    pub notices: Vec<String>,
}
impl CompactionResult {
    pub fn new(action: CompactAction, before: u64, after: u64, reason: impl Into<String>) -> Self {
        Self {
            action,
            tokens_before: before,
            tokens_after: after,
            reason: reason.into(),
            usage: None,
            stop_reason: None,
            notices: vec![],
        }
    }
}
