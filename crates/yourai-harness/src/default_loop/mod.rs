//! Default scheduling policy. Hooks belong to the business operation wrappers.
mod config;
use crate::execution::{Completion, ModelOptions, TurnExecution};
pub use config::{AttachmentImageConfig, LoopConfig};
use yourai_core::prelude::*;
// Verbatim from opencode `packages/core/src/session/runner/max-steps.ts`.
pub(super) const MAX_STEPS_PROMPT: &str = r#"CRITICAL - MAXIMUM STEPS REACHED

The maximum number of steps allowed for this task has been reached. Tools are disabled until next user input. Respond with text only.

STRICT REQUIREMENTS:
1. Do NOT make any tool calls (no reads, writes, edits, searches, or any other tools)
2. MUST provide a text response summarizing work done so far
3. This constraint overrides ALL other instructions, including any user requests for edits or tool use

Response must include:
- Statement that maximum steps for this agent have been reached
- Summary of what has been accomplished so far
- List of any remaining tasks that were not completed
- Recommendations for what should be done next

Any attempt to use tools is a critical violation. Respond with text ONLY."#;

/// Defensive bound on forced-final iterations that still return tool calls.
pub(super) const MAX_FORCED_FINAL_CONTINUATIONS: u32 = 3;

#[derive(Debug, Clone, Default)]
pub struct DefaultLoop {
    config: LoopConfig,
}
impl DefaultLoop {
    pub fn new(config: LoopConfig) -> Self {
        Self { config }
    }
    pub fn config(&self) -> &LoopConfig {
        &self.config
    }
}
impl AgentLoop for DefaultLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let steps = match tc.info.options.limits.steps {
                Some(limit) => Some(
                    self.config
                        .steps
                        .map_or(limit, |configured| configured.min(limit)),
                ),
                None => self.config.steps,
            };
            let mut execution = TurnExecution::open(tc, self.config.execution.clone()).await?;
            let result = async {
                if !execution.input_accepted() {
                    return Ok(());
                }
                if steps == Some(0) {
                    return Err(ErrorKind::Config("steps must be a positive integer".into()).into());
                }
                let mut step = 0u32;
                let mut forced_final = false;
                let mut continuations = 0;
                loop {
                    execution.checkpoint().await?;
                    step = step.saturating_add(1);
                    forced_final |= steps.is_some_and(|max| step >= max);
                    let response = execution
                        .model()
                        .exec_with(ModelOptions {
                            tools_enabled: !forced_final,
                            prefill: forced_final.then(|| MAX_STEPS_PROMPT.into()),
                        })
                        .await?;
                    if response.calls.is_empty() {
                        if execution.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else if forced_final {
                        // opencode `failUnsettledTools`: record explicit
                        // failure results so the model can see why its calls
                        // were refused and produce the required text-only
                        // summary. A provider honoring tool_choice=none never
                        // reaches this; the bound only guards a misbehaving
                        // one. reject_pending also emits one batch warning.
                        execution
                            .tools()
                            .reject_pending("Tools are disabled after the maximum agent steps")
                            .await?;
                        if continuations >= MAX_FORCED_FINAL_CONTINUATIONS {
                            // Bound exhausted (announced once): complete with
                            // the text we have instead of asking the model
                            // again. Stop hooks run here — the earlier loop
                            // bailed out without them — and a completion that
                            // cannot settle surfaces as a turn error, not
                            // silent success.
                            if continuations == MAX_FORCED_FINAL_CONTINUATIONS {
                                execution.notice(
                                    Level::Warning,
                                    "Model requested tools after maximum agent steps; they were not executed.",
                                )?;
                            }
                            if execution.complete(response.text).await?
                                == Completion::Completed
                            {
                                return Ok(());
                            }
                        }
                        continuations += 1;
                    } else {
                        execution.tools().exec_pending().await?;
                    }
                    tokio::task::yield_now().await;
                }
            }
            .await;
            execution.finish(result).await
        })
    }
}
