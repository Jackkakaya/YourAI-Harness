//! Model capacity and generation settings have one owner: the selected model.
use crate::{ErrorKind, YourAiError};

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, serde::Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct ModelLimits {
    pub context: Option<u64>,
    pub input: Option<u64>,
    pub output: Option<u32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ModelTokenBudget {
    limits: ModelLimits,
    output: u32,
}
impl Default for ModelTokenBudget {
    fn default() -> Self {
        Self {
            limits: ModelLimits::default(),
            output: 32_000,
        }
    }
}
impl ModelTokenBudget {
    pub fn resolve(limits: ModelLimits, requested: Option<u32>) -> Result<Self, YourAiError> {
        if limits.context == Some(0) || limits.input == Some(0) || limits.output == Some(0) {
            return Err(ErrorKind::Config("model limits must be positive".into()).into());
        }
        let budget = Self {
            limits,
            output: requested.or(limits.output).unwrap_or(32_000),
        };
        budget.validate_output(budget.output)?;
        Ok(budget)
    }
    pub fn limits(self) -> ModelLimits {
        self.limits
    }
    pub fn max_output_tokens(self) -> u32 {
        self.output
    }
    pub fn validate_output(self, output: u32) -> Result<(), YourAiError> {
        if output == 0
            || self.limits.output.is_some_and(|limit| output > limit)
            || self
                .limits
                .context
                .is_some_and(|limit| u64::from(output) >= limit)
        {
            return Err(ErrorKind::Config("output budget must be positive, within model output capacity, and leave room for input".into()).into());
        }
        Ok(())
    }
    pub fn input_budget(self, margin: u64) -> Option<u64> {
        let shared = self
            .limits
            .context
            .map(|n| n.saturating_sub(u64::from(self.output)));
        match (self.limits.input, shared) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
        .map(|n| n.saturating_sub(margin))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn output_precedence_and_independent_input_limits() {
        let limits = ModelLimits {
            context: Some(300_000),
            input: Some(100_000),
            output: Some(131_072),
        };
        let budget = ModelTokenBudget::resolve(limits, None).unwrap();
        assert_eq!(budget.max_output_tokens(), 131_072);
        assert_eq!(budget.input_budget(1024), Some(98_976));
        assert_eq!(
            ModelTokenBudget::resolve(limits, Some(65_536))
                .unwrap()
                .max_output_tokens(),
            65_536
        );
        assert_eq!(
            ModelTokenBudget::resolve(ModelLimits::default(), None)
                .unwrap()
                .max_output_tokens(),
            32_000
        );
        assert!(ModelTokenBudget::resolve(limits, Some(131_073)).is_err());
        assert!(ModelTokenBudget::resolve(limits, Some(0)).is_err());
        assert!(ModelTokenBudget::resolve(
            ModelLimits {
                context: Some(32_000),
                ..Default::default()
            },
            None
        )
        .is_err());
    }
}
