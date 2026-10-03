//! Shared presentation for manual and automatic context maintenance.
use super::{CompactAction, CompactionEvent, Level, View};

impl View {
    pub(super) fn compaction_event(&mut self, event: CompactionEvent) {
        self.session.context_revision = self.session.context_revision.wrapping_add(1);
        match event {
            CompactionEvent::Progress { trigger, phase } => {
                self.session.compaction = Some((trigger, phase));
                self.session.retry = None;
            }
            CompactionEvent::Finished { result, .. } => {
                self.session.compaction = None;
                if result.verified {
                    if let Some(usage) = &mut self.session.context_usage {
                        usage.estimated_tokens = result.tokens_after;
                        usage.input_budget = result.input_budget;
                    }
                } else {
                    self.session.context_usage = None;
                }
                let action = match result.action {
                    CompactAction::Unchanged => "Context unchanged",
                    CompactAction::Pruned => "Old tool output cleared",
                    CompactAction::Summarized => "Context compacted",
                };
                let message = if result.action == CompactAction::Unchanged {
                    format!("{action} · {}", result.reason)
                } else if result.verified {
                    format!(
                        "{action} · ~{} → ~{} tokens · {} messages kept",
                        result.tokens_before, result.tokens_after, result.retained_messages
                    )
                } else {
                    format!("{action} · usage unavailable")
                };
                self.notice(
                    if result.stop_reason.is_some() {
                        Level::Warning
                    } else {
                        Level::Info
                    },
                    message,
                );
                for notice in &result.notices {
                    self.notice(Level::Info, notice.clone());
                }
                if let Some(reason) = &result.stop_reason {
                    self.notice(Level::Warning, reason.clone());
                }
                self.session.last_compaction = Some(result);
            }
            CompactionEvent::Failed {
                message, committed, ..
            } => {
                self.session.compaction = None;
                self.session.context_usage = None;
                self.notice(
                    Level::Warning,
                    format!(
                        "{}: {message}",
                        if committed {
                            "Context saved; maintenance stopped"
                        } else {
                            "Context maintenance stopped"
                        }
                    ),
                );
            }
        }
        self.touch();
    }
}

#[cfg(test)]
mod tests {
    use super::{CompactAction, CompactionEvent, Level, View};
    use crate::ui::state::Item;
    use yourai_core::prelude::{CompactionPhase, CompactionResult, CompactionTrigger};

    #[test]
    fn manual_and_auto_use_one_presentation_and_keep_committed_stops_visible() {
        for trigger in [
            CompactionTrigger::Manual,
            CompactionTrigger::Threshold,
            CompactionTrigger::Overflow,
        ] {
            let mut view = View::default();
            view.compaction_event(CompactionEvent::Progress {
                trigger,
                phase: CompactionPhase::Summarizing,
            });
            assert_eq!(
                view.session.compaction,
                Some((trigger, CompactionPhase::Summarizing))
            );
            assert!(view.items().is_empty());
            let mut result = CompactionResult::new(CompactAction::Summarized, 10000, 500, "saved");
            result.verified = true;
            result.retained_messages = 3;
            result.stop_reason = Some("PostCompact stopped continuation".into());
            result.notices = vec!["hook notice".into()];
            view.compaction_event(CompactionEvent::Finished { trigger, result });
            assert!(view.session.compaction.is_none());
            assert_eq!(view.session.context_revision, 2);
            assert_eq!(view.items().len(), 3);
            assert!(
                matches!(&view.items()[0], Item::Notice { level: Level::Warning, text }
                if text.contains("~10000 → ~500") && text.contains("3 messages kept"))
            );
            assert!(
                matches!(&view.items()[2], Item::Notice { text, .. } if text.contains("stopped continuation"))
            );
        }
    }
}
