//! Context maintenance policy and committed outcomes.
use crate::chat::{GenaiUsage, Tool};
use std::{
    sync::{atomic::AtomicU32, Arc},
    time::Instant,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompactionTrigger {
    Threshold,
    Overflow,
    Manual,
}

#[derive(Debug, Clone, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ContextPolicy {
    pub safety_margin: u64,
    pub advance_tokens: u64,
    pub keep_recent_tokens: u64,
    /// Prefer at most this many recent turns, bounded by keep_recent_tokens.
    pub keep_recent_turns: usize,
    pub summary_tokens: u64,
    /// Summary input previews have a separate, smaller budget than normal requests.
    pub summary_tool_output_chars: usize,
    pub tool_output_chars: usize,
    pub prune_enabled: bool,
    pub prune_min_savings: u64,
    pub prune_growth: u64,
    pub summary_min_savings: u64,
}
impl Default for ContextPolicy {
    fn default() -> Self {
        Self {
            safety_margin: 1024,
            advance_tokens: 4096,
            keep_recent_tokens: 8_000,
            keep_recent_turns: 2,
            summary_tokens: 2000,
            summary_tool_output_chars: 2000,
            tool_output_chars: 16_000,
            prune_enabled: true,
            prune_min_savings: 1024,
            prune_growth: 2048,
            summary_min_savings: 256,
        }
    }
}
impl ContextPolicy {
    pub fn validate(&self) -> Result<(), crate::YourAiError> {
        if self.summary_tokens == 0
            || self.tool_output_chars < 256
            || self.summary_tool_output_chars < 256
        {
            return Err(crate::ErrorKind::Config("invalid context policy: require a positive summary budget and tool_output_chars and summary_tool_output_chars >= 256".into()).into());
        }
        Ok(())
    }
    pub fn validate_for(
        &self,
        budget: crate::model::ModelTokenBudget,
    ) -> Result<(), crate::YourAiError> {
        self.validate()?;
        if self.input_budget(budget) == Some(0) {
            return Err(crate::ErrorKind::Config(
                "no available model input after output budget and safety margin".into(),
            )
            .into());
        }
        Ok(())
    }
    pub fn input_budget(&self, budget: crate::model::ModelTokenBudget) -> Option<u64> {
        budget.input_budget(self.safety_margin)
    }
    /// Start maintenance early enough to leave room for another interaction.
    pub fn maintenance_threshold(&self, budget: crate::model::ModelTokenBudget) -> Option<u64> {
        self.input_budget(budget)
            .map(|n| n.saturating_sub(self.advance_tokens))
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
    /// Compaction may itself need multiple model calls (chunked summarizing);
    /// the cap keeps a pathological loop from summarizing forever.
    pub const DEFAULT_MAX_MODEL_CALLS: u32 = 8;

    pub fn new(trigger: CompactionTrigger) -> Self {
        Self {
            trigger,
            custom_instructions: None,
            target_tokens: None,
            deadline: None,
            tools: vec![],
            calls: Arc::new(AtomicU32::new(0)),
            max_model_calls: Self::DEFAULT_MAX_MODEL_CALLS,
            usage: Arc::new(std::sync::Mutex::new(vec![])),
        }
    }
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompactAction {
    Unchanged,
    Pruned,
    Summarized,
}
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
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
    /// True only after rebuilding the request, including post-compact context.
    pub verified: bool,
    pub input_budget: Option<u64>,
    pub summarized_messages: usize,
    pub retained_messages: usize,
    pub pruned_outputs: usize,
    pub model_calls: u32,
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
            verified: false,
            input_budget: None,
            summarized_messages: 0,
            retained_messages: 0,
            pruned_outputs: 0,
            model_calls: 0,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum CompactionPhase {
    Preparing,
    Summarizing,
    Rebuilding,
}

/// One lifecycle for manual, threshold and overflow maintenance.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum CompactionEvent {
    Progress {
        trigger: CompactionTrigger,
        phase: CompactionPhase,
    },
    Finished {
        trigger: CompactionTrigger,
        result: CompactionResult,
    },
    Failed {
        trigger: CompactionTrigger,
        message: String,
        committed: bool,
    },
}
