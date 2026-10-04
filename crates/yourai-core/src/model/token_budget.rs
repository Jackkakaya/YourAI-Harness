//! Model capacity and the resolved output budget, independent of reasoning effort.
use crate::{ErrorKind, YourAiError};

/// Configured model capacities, independent of request options. Unknown stays unknown.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelLimits {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u32>,
}

/// A validated selection shared by generation and input admission.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTokenBudget {
    limits: ModelLimits,
    max_output_tokens: u32,
}

impl Default for ModelTokenBudget {
    fn default() -> Self {
        Self {
            limits: ModelLimits::default(),
            max_output_tokens: 32_000,
        }
    }
}

impl ModelTokenBudget {
    pub fn resolve(limits: ModelLimits, requested: Option<u32>) -> Result<Self, YourAiError> {
        if limits.context == Some(0) || limits.input == Some(0) || limits.output == Some(0) {
            return Err(ErrorKind::Config("Model limits must be positive".into()).into());
        }
        let budget = Self {
            limits,
            max_output_tokens: requested
                .or(limits.output)
                .unwrap_or(Self::default().max_output_tokens),
        };
        budget.validate_output(budget.max_output_tokens)?;
        Ok(budget)
    }

    pub fn limits(self) -> ModelLimits {
        self.limits
    }
    pub fn max_output_tokens(self) -> u32 {
        self.max_output_tokens
    }

    /// Per-operation overrides (for example summaries) obey the same capacities.
    pub fn validate_output(self, tokens: u32) -> Result<(), YourAiError> {
        if tokens == 0 {
            return Err(ErrorKind::Config("maxOutputTokens must be positive".into()).into());
        }
        if self.limits.output.is_some_and(|limit| tokens > limit) {
            return Err(
                ErrorKind::Config("maxOutputTokens exceeds model limit.output".into()).into(),
            );
        }
        if self
            .limits
            .context
            .is_some_and(|limit| u64::from(tokens) >= limit)
        {
            return Err(
                ErrorKind::Config("output budget leaves no room for model input".into()).into(),
            );
        }
        Ok(())
    }

    pub fn input_budget(self, safety_margin: u64) -> Option<u64> {
        let shared = self
            .limits
            .context
            .map(|n| n.saturating_sub(u64::from(self.max_output_tokens)));
        match (self.limits.input, shared) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|n| n.saturating_sub(safety_margin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_selection_has_no_universal_ceiling() {
        for (output, requested, expected) in [
            (Some(131072), None, 131072),
            (Some(16384), None, 16384),
            (Some(131072), Some(65536), 65536),
            (None, Some(262144), 262144),
            (None, None, 32000),
        ] {
            let budget = ModelTokenBudget::resolve(
                ModelLimits {
                    output,
                    ..Default::default()
                },
                requested,
            )
            .unwrap();
            assert_eq!(budget.max_output_tokens(), expected);
        }
    }

    #[test]
    fn input_capacity_and_shared_context_are_independent_constraints() {
        for (context, input, expected) in [
            (Some(300000), None, Some(167904)),
            (None, Some(200000), Some(198976)),
            (Some(300000), Some(200000), Some(167904)),
            (Some(300000), Some(100000), Some(98976)),
            (None, None, None),
        ] {
            let budget = ModelTokenBudget::resolve(
                ModelLimits {
                    context,
                    input,
                    output: Some(131072),
                },
                None,
            )
            .unwrap();
            assert_eq!(budget.input_budget(1024), expected);
        }
    }

    #[test]
    fn invalid_limits_and_output_are_rejected_without_clamping() {
        for limits in [
            ModelLimits {
                context: Some(0),
                ..Default::default()
            },
            ModelLimits {
                input: Some(0),
                ..Default::default()
            },
            ModelLimits {
                output: Some(0),
                ..Default::default()
            },
            ModelLimits {
                context: Some(32000),
                ..Default::default()
            },
        ] {
            assert!(ModelTokenBudget::resolve(limits, None).is_err());
        }
        let limits = ModelLimits {
            context: Some(200000),
            output: Some(131072),
            ..Default::default()
        };
        assert!(ModelTokenBudget::resolve(limits, Some(0)).is_err());
        assert!(ModelTokenBudget::resolve(limits, Some(131073)).is_err());
        let budget = ModelTokenBudget::resolve(limits, Some(65536)).unwrap();
        assert!(budget.validate_output(2000).is_ok());
        assert!(budget.validate_output(131073).is_err());
        let policy = crate::compaction::ContextPolicy {
            safety_margin: 200000,
            ..Default::default()
        };
        assert!(policy.validate_for(budget).is_err());
    }
}
