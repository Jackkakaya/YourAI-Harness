use super::*;

pub(super) struct SummaryPlan {
    pub(super) system: String,
    pub(super) records: Vec<StoredMessage>,
    pub(super) projected: Vec<StoredMessage>,
    pub(super) selected: Vec<std::ops::Range<usize>>,
    pub(super) pruned: Vec<String>,
    pub(super) before: u64,
    pub(super) raw_before: u64,
    pub(super) budget: Option<u64>,
    pub(super) summary_budget: u64,
    pub(super) fingerprint: String,
}

impl MemoryContext {
    pub(super) async fn prepare(
        &self,
        options: &CompactionRequest,
        execution: &ContextExecution,
        _cancel: &CancellationToken,
    ) -> Result<CompactionPlan<'_>, YourAiError> {
        let guard = self.gate.lock().await;
        self.recover().await?;
        let system = self.system_prompt();
        let records = self.records();
        let policy = &self.services.policy;
        policy.validate_for(execution.model().token_budget())?;
        let before = self.estimate(
            &self.project(&records, Some(system.as_str()), &options.tools)?,
            execution,
        )?;
        let budget = policy.input_budget(execution.model().token_budget());
        let threshold = policy.maintenance_threshold(execution.model().token_budget());
        let fingerprint = Self::fingerprint(
            &records,
            Some(system.as_str()),
            RequestInput::tools(&options.tools),
            execution.model().as_ref(),
        );
        let unchanged = |reason| {
            let mut result =
                CompactionResult::new(CompactAction::Unchanged, before, before, reason);
            result.retained_messages = records.len();
            result
        };
        if options.trigger == CompactionTrigger::Threshold
            && (threshold.is_some_and(|t| before < t)
                || self.view.lock().unwrap().last_maintenance.as_ref() == Some(&fingerprint))
        {
            return Ok(CompactionPlan::Complete(unchanged(
                "below threshold or already maintained this view",
            )));
        }
        // A configured input budget is needed for bounded summarizer requests too.
        let summary_budget = budget.ok_or_else(|| {
            error(
                "compact",
                "configure model limit.context or limit.input before summarizing",
            )
        })?;
        if summary_budget == 0 {
            return Err(error("compact", "no input budget after reserves"));
        }
        let keep = if before > summary_budget {
            0
        } else {
            policy.keep_recent_tokens.min(
                summary_budget
                    / if options.trigger == CompactionTrigger::Overflow {
                        8
                    } else {
                        3
                    },
            )
        };
        let safe = selection::groups(&records);
        let tail_start = selection::tail_start(self, &records, &safe, keep, execution)?;
        let latest_user = records
            .iter()
            .rposition(|m| !m.summary && !m.runtime_context && m.message.role == ChatRole::User);
        let selected: Vec<_> = safe
            .iter()
            .filter(|g| g.end <= tail_start && !latest_user.is_some_and(|i| g.contains(&i)))
            .cloned()
            .collect();
        let mut projected = records.clone();
        let mut pruned = vec![];
        if policy.prune_enabled && options.trigger != CompactionTrigger::Manual {
            let view = self.view.lock().unwrap();
            let growth = before.saturating_sub(view.last_prune_tokens);
            if growth >= policy.prune_growth {
                for g in &selected {
                    for i in g.clone() {
                        let m = &mut projected[i];
                        let results = m.message.content.tool_responses();
                        if m.tool_output_pruned_at.is_none()
                            && results.len() == 1
                            && view.consumed.contains(&results[0].call_id)
                        {
                            // Unknown/error/denied/cancelled outputs stay intact. Tools opt in
                            // through an explicit successful status and may protect their output.
                            let value: serde_json::Value =
                                serde_json::from_str(&results[0].content).unwrap_or_default();
                            if value.get("ok").and_then(|v| v.as_bool()) == Some(true)
                                && value.get("error").is_none_or(|v| v.is_null())
                                && value.get("preserve_context").and_then(|v| v.as_bool())
                                    != Some(true)
                            {
                                m.tool_output_pruned_at = Some(
                                    std::time::SystemTime::now()
                                        .duration_since(std::time::UNIX_EPOCH)
                                        .unwrap_or_default()
                                        .as_millis() as i64,
                                );
                                pruned.push(m.id.clone());
                            }
                        }
                    }
                }
            }
        }
        let raw_before = self.estimate_raw(
            &self.project(&records, Some(system.as_str()), &options.tools)?,
            execution,
        )?;
        let after_prune = self.estimate_raw(
            &self.project(&projected, Some(system.as_str()), &options.tools)?,
            execution,
        )?;
        if raw_before.saturating_sub(after_prune) < policy.prune_min_savings {
            projected = records.clone();
            pruned.clear();
        }
        if !pruned.is_empty() && threshold.is_some_and(|t| after_prune < t) {
            let pruned_outputs = pruned.len();
            self.commit(ContextChange {
                compaction: None,
                pruned,
            })
            .await?;
            let mut view = self.view.lock().unwrap();
            view.last_prune_tokens = after_prune;
            view.last_maintenance = Some(Self::fingerprint(
                &view.records,
                Some(system.as_str()),
                RequestInput::tools(&options.tools),
                execution.model().as_ref(),
            ));
            let mut result = CompactionResult::new(
                CompactAction::Pruned,
                before,
                after_prune,
                "old successful tool outputs omitted",
            );
            result.pruned_outputs = pruned_outputs;
            result.retained_messages = records.len();
            return Ok(CompactionPlan::Complete(result));
        }
        if selected.is_empty() {
            return Ok(CompactionPlan::Complete(unchanged(
                "no safe history outside protected messages",
            )));
        }
        Ok(CompactionPlan::Summary(Box::new(SummaryJob {
            context: self,
            _guard: guard,
            plan: SummaryPlan {
                system,
                records,
                projected,
                selected,
                pruned,
                before,
                budget,
                summary_budget,
                fingerprint,
                raw_before,
            },
        })))
    }
}

struct SummaryJob<'a> {
    context: &'a MemoryContext,
    _guard: tokio::sync::MutexGuard<'a, ()>,
    plan: SummaryPlan,
}
impl CompactionJob for SummaryJob<'_> {
    fn run<'a>(
        self: Box<Self>,
        options: CompactionRequest,
        execution: &'a ContextExecution,
        cancel: &'a CancellationToken,
        committed: &'a AtomicBool,
    ) -> BoxFuture<'a, Result<CompactionCommit, YourAiError>>
    where
        Self: 'a,
    {
        Box::pin(async move {
            let (result, summary) = self
                .context
                .run_summary(&options, execution, cancel, committed, self.plan)
                .await?;
            Ok(CompactionCommit { result, summary })
        })
    }
}
