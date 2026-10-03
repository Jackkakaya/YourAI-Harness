//! Fixed compaction operation for all ContextManager implementations.
use super::*;
use std::time::Duration;

async fn hook(
    execution: &ContextExecution,
    event: HookEvent,
    timeout: Option<Duration>,
) -> Result<HookDispatchResult, YourAiError> {
    let kind = event.kind();
    let result = match &execution.hooks {
        Some(hooks) => crate::time::timeout(
            timeout,
            hooks.dispatch(&HookInvocation::new(execution.hook_base.clone(), event)),
        )
        .await
        .map_err(|_| error("compact", "hook deadline exceeded"))??,
        None => HookDispatchResult::empty(kind),
    };
    result.validate_for(kind)?;
    Ok(result)
}
fn additional(r: &HookDispatchResult) -> Vec<String> {
    match &r.outcome {
        HookPointOutcome::Generic(o) => o.additional_contexts.clone(),
        _ => vec![],
    }
}
/// Silent entry point for callers interested only in the verified outcome.
pub async fn compact(
    context: &dyn ContextManager,
    options: CompactionRequest,
    execution: &ContextExecution,
    cancel: &CancellationToken,
    hook_timeout: Option<Duration>,
) -> Result<CompactionResult, YourAiError> {
    compact_with_events(
        context,
        options,
        execution,
        cancel,
        hook_timeout,
        &yourai_core::context::DiscardSink,
    )
    .await
}

/// Owns the full maintenance lifecycle for every entry point. A terminal event
/// also fires when an execution supervisor drops this future during cancellation.
struct Lifecycle<'a> {
    events: &'a dyn OutSink,
    trigger: CompactionTrigger,
    committed: &'a AtomicBool,
    finished: bool,
}
impl Lifecycle<'_> {
    fn phase(&self, phase: CompactionPhase) -> Result<(), YourAiError> {
        if self.events.send(Out::Compaction {
            event: CompactionEvent::Progress {
                trigger: self.trigger,
                phase,
            },
        }) {
            Ok(())
        } else {
            Err(AbortReason::Disconnected.into())
        }
    }
}
impl Drop for Lifecycle<'_> {
    fn drop(&mut self) {
        if !self.finished {
            self.events.send(Out::Compaction {
                event: CompactionEvent::Failed {
                    trigger: self.trigger,
                    message: "Context maintenance interrupted; restore before continuing".into(),
                    committed: self.committed.load(Ordering::Acquire),
                },
            });
        }
    }
}

pub async fn compact_with_events(
    context: &dyn ContextManager,
    options: CompactionRequest,
    execution: &ContextExecution,
    cancel: &CancellationToken,
    hook_timeout: Option<Duration>,
    events: &dyn OutSink,
) -> Result<CompactionResult, YourAiError> {
    let committed = AtomicBool::new(false);
    let mut lifecycle = Lifecycle {
        events,
        trigger: options.trigger,
        committed: &committed,
        finished: false,
    };
    let checkpoint = Mutex::new(None::<CompactionResult>);
    let operation = async {
        lifecycle.phase(CompactionPhase::Preparing)?;
        let plan = context
            .prepare_compaction(&options, execution, cancel)
            .await?;
        let job = match plan {
            CompactionPlan::Complete(result) => {
                if result.action == CompactAction::Summarized {
                    return Err(ErrorKind::Config(
                        "summary commits must use a CompactionJob".into(),
                    )
                    .into());
                }
                committed.store(result.action == CompactAction::Pruned, Ordering::Release);
                return Ok(result);
            }
            CompactionPlan::Summary(job) => job,
        };
        lifecycle.phase(CompactionPhase::Summarizing)?;
        let trigger = match options.trigger {
            CompactionTrigger::Manual => "manual",
            CompactionTrigger::Threshold => "auto",
            CompactionTrigger::Overflow => "overflow",
        };
        let pre = hook(
            execution,
            HookEvent::PreCompact {
                trigger: trigger.into(),
                custom_instructions: options.custom_instructions.clone(),
            },
            hook_timeout,
        )
        .await?;
        if pre.common.prevent_continuation {
            return Err(AbortReason::HookStopped(
                pre.common
                    .stop_reason
                    .clone()
                    .unwrap_or_else(|| "PreCompact stopped".into()),
            )
            .into());
        }
        if !pre.common.blocking_errors.is_empty() {
            return Err(error("compact", "PreCompact blocked summary"));
        }
        let mut run_options = options.clone();
        let extra = additional(&pre);
        if !extra.is_empty() {
            run_options.custom_instructions = Some(
                [
                    run_options.custom_instructions.unwrap_or_default(),
                    extra.join("\n"),
                ]
                .join("\n"),
            );
        }
        let CompactionCommit {
            mut result,
            summary,
        } = job.run(run_options, execution, cancel, &committed).await?;
        committed.store(true, Ordering::Release);
        result.notices.extend(pre.notices().map(str::to_owned));
        *checkpoint.lock().unwrap() = Some(result.clone());
        lifecycle.phase(CompactionPhase::Rebuilding)?;
        match hook(
            execution,
            HookEvent::PostCompact {
                trigger: trigger.into(),
                compact_summary: summary,
            },
            hook_timeout,
        )
        .await
        {
            Ok(post) => {
                result.notices.extend(post.notices().map(str::to_owned));
                let extra = additional(&post);
                if !extra.is_empty() {
                    if let Err(e) = context
                        .append(vec![StoredMessage::runtime_context(format!(
                            "[PostCompact context]\n{}",
                            extra.join("\n")
                        ))])
                        .await
                    {
                        result.stop_reason = Some(format!(
                            "summary committed; PostCompact context save failed: {e}"
                        ));
                    }
                }
                if post.common.prevent_continuation || !post.common.blocking_errors.is_empty() {
                    result.stop_reason = Some(post.common.stop_reason.unwrap_or_else(|| {
                        "summary committed; PostCompact stopped continuation".into()
                    }));
                }
            }
            Err(e) => {
                result.stop_reason = Some(format!("summary committed; PostCompact failed: {e}"))
            }
        }
        Ok(result)
    };
    let outcome = tokio::select! {
        biased;
        _ = cancel.cancelled() => Err(YourAiError::from(AbortReason::Cancelled)),
        _ = events.closed() => Err(YourAiError::from(AbortReason::Disconnected)),
        _ = crate::time::sleep_until(options.deadline) => Err(YourAiError::from(AbortReason::DeadlineExceeded)),
        result = operation => result,
    };
    let outcome = match outcome {
        Err(e) => match checkpoint.into_inner().unwrap() {
            Some(mut result) => {
                result.stop_reason = Some(format!("summary committed; continuation stopped: {e}"));
                Ok(result)
            }
            None => Err(e),
        },
        result => result,
    };
    let outcome = outcome.map(|mut result| {
        // Rebuild from committed history using the same system and tool definitions
        // as the next model request. PostCompact additions are part of this budget.
        match context.build_request(&options.tools, execution) {
            Ok(request) => {
                result.tokens_after = request.estimated_tokens;
                result.input_budget = request.input_budget;
                result.verified = true;
                if !request.fits() {
                    let reason = format!(
                        "context needs {} tokens but input budget is {}; continuation stopped",
                        request.estimated_tokens,
                        request.input_budget.unwrap_or(0)
                    );
                    result.stop_reason = Some(match result.stop_reason {
                        Some(existing) => format!("{existing}; {reason}"),
                        None => reason,
                    });
                }
            }
            Err(e) => {
                result.verified = false;
                result.stop_reason = Some(format!(
                    "cannot verify committed context; restore required: {e}"
                ));
            }
        }
        result.model_calls = options.calls.load(Ordering::Acquire);
        result
    });
    let event = match &outcome {
        Ok(result) => CompactionEvent::Finished {
            trigger: options.trigger,
            result: result.clone(),
        },
        Err(e) => CompactionEvent::Failed {
            trigger: options.trigger,
            message: e.to_string(),
            committed: committed.load(Ordering::Acquire),
        },
    };
    events.send(Out::Compaction { event });
    lifecycle.finished = true;
    outcome
}
