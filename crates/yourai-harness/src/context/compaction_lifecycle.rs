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
/// Plan -> before hook -> business summary/commit -> after hook.
/// Unchanged and prune-only operations never trigger summary hooks.
pub async fn compact(
    context: &dyn ContextManager,
    options: CompactionRequest,
    execution: &ContextExecution,
    cancel: &CancellationToken,
    hook_timeout: Option<Duration>,
) -> Result<CompactionResult, YourAiError> {
    let committed = AtomicBool::new(false);
    let deadline = options.deadline;
    let operation = async {
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
                return Ok(result);
            }
            CompactionPlan::Summary(job) => job,
        };
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
        if result.action != CompactAction::Summarized {
            return Err(ErrorKind::Config(
                "CompactionJob must report a Summarized commit; the summary is durable, \
                 but its reported action breaks overflow accounting"
                    .into(),
            )
            .into());
        }
        result.notices.extend(pre.notices().map(str::to_owned));
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
                let additional = additional(&post);
                if !additional.is_empty() {
                    if let Err(e) = context
                        .append(vec![StoredMessage::runtime_context(format!(
                            "[PostCompact context]\n{}",
                            additional.join("\n")
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
    tokio::select! {
        biased;
        _ = cancel.cancelled() => if committed.load(Ordering::Acquire) { Err(error("compact", "summary committed; cancelled during PostCompact")) } else { Err(AbortReason::Cancelled.into()) },
        _ = crate::time::sleep_until(deadline) => if committed.load(Ordering::Acquire) { Err(error("compact", "summary committed; deadline exceeded during PostCompact")) } else { Err(AbortReason::DeadlineExceeded.into()) },
        result = operation => result,
    }
}
