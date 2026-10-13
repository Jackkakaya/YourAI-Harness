//! Default scheduling policy. Hooks belong to the business operation wrappers.

use crate::execution::{Completion, ModelOptions, Turn};
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
pub struct DefaultLoop;
impl AgentLoop for DefaultLoop {
    fn run_turn<'a>(&'a self, tc: TurnContext<'a>) -> BoxFuture<'a, TurnResult> {
        Box::pin(async move {
            let mut execution = Turn::open(tc).await?;
            let steps = execution.config().steps;
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
                        .model()?
                        .exec(
                            &mut execution,
                            ModelOptions {
                                tools_enabled: !forced_final,
                                prefill: forced_final.then(|| MAX_STEPS_PROMPT.into()),
                            },
                        )
                        .await?;
                    if response.calls.is_empty() {
                        if execution.complete(response.text).await? == Completion::Completed {
                            return Ok(());
                        }
                    } else if forced_final {
                        execution
                            .reject_pending_tools(
                                "Tools are disabled after the maximum agent steps",
                            )
                            .await?;
                        if continuations >= MAX_FORCED_FINAL_CONTINUATIONS
                            && execution.complete(response.text).await? == Completion::Completed
                        {
                            return Ok(());
                        }
                        continuations += 1;
                    } else {
                        for call in execution.pending_tools() {
                            execution
                                .tool(&call.call_id)?
                                .exec(&mut execution, &call.call_id)
                                .await?;
                        }
                    }
                    tokio::task::yield_now().await;
                }
            }
            .await;
            execution.finish(result).await
        })
    }
}
