//! User-input admission owns preparation, explicit rejection and history commit.
//! No input leaves the pending queue until either rejection is delivered or the
//! history write succeeds. Transient provider/storage failures retain ownership.
use super::{attachment, Turn};
use crate::prelude::*;

impl Turn<'_> {
    pub(crate) async fn accept_queued_input(
        &mut self,
        index: usize,
        initial: bool,
    ) -> Result<bool, YourAiError> {
        let text = match &self.queued[index] {
            In::UserText { text, .. } => text.clone(),
            _ => return Ok(false),
        };
        let id = match &self.queued[index] {
            In::UserText { id: Some(id), .. } if !id.is_empty() => id.clone(),
            _ => return Err(ErrorKind::Config("missing input identity".into()).into()),
        };
        // Settle a previous write before deciding whether this is a retry.
        if !initial {
            let history = self.history.clone();
            self.wait_operation(history.restore(), self.op_timeout(), "history")
                .await?;
        }
        if let Some(record) = self.history.records().into_iter().find(|r| r.id == id) {
            if record.runtime_context
                || record.summary
                || record.message.role != ChatRole::User
                || record
                    .input
                    .as_ref()
                    .map(serde_json::to_value)
                    .transpose()
                    .map_err(|e| ErrorKind::Loop(e.to_string()))?
                    != Some(
                        serde_json::to_value(&self.queued[index])
                            .map_err(|e| ErrorKind::Loop(e.to_string()))?,
                    )
            {
                return Err(ErrorKind::Loop("input identity conflict".into()).into());
            }
            // The user and hook contexts were already committed atomically.
            self.queued.remove(index);
            self.repeated_tool = None;
            return Ok(true);
        }
        let hook = self
            .hook(HookEvent::UserPromptSubmit {
                prompt: text.clone(),
            })
            .await?;
        self.apply_common(&hook)?;
        if !hook.common.blocking_errors.is_empty() {
            return self.reject_input(index, hook.blocking_messages().join("\n"));
        }
        self.run_accept_input(index, initial, hook.additional_contexts())
            .await
    }
    async fn run_accept_input(
        &mut self,
        index: usize,
        initial: bool,
        contexts: &[String],
    ) -> Result<bool, YourAiError> {
        let (text, attachments) = match &self.queued[index] {
            In::UserText {
                text, attachments, ..
            } => (text.clone(), attachments.clone()),
            _ => return Ok(false),
        };
        let history = self.history.clone();
        let prepared = if attachments.is_empty() {
            Ok(ChatMessage::user(&text))
        } else {
            let cwd = self.tc.info.options.session.as_ref().map(|s| s.cwd.clone());
            let config = self.config.clone();
            let prompt = text.clone();
            // File I/O and image decoding must not block control handling.
            // A cancelled worker may finish its read, but cannot commit history.
            self.wait_operation(
                async move {
                    tokio::task::spawn_blocking(move || {
                        attachment::message(&prompt, &attachments, &config, cwd.as_deref())
                    })
                    .await
                    .map_err(|e| {
                        ErrorKind::Provider {
                            name: "attachment",
                            message: e.to_string(),
                        }
                        .into()
                    })
                },
                self.op_timeout(),
                "attachment",
            )
            .await?
        };
        let message = match prepared {
            Ok(message) => message,
            Err(attachment::ResolveError::Invalid(reason)) => {
                return self.reject_input(index, reason)
            }
            Err(attachment::ResolveError::Unavailable(error)) => return Err(error),
        };
        let mut record = StoredMessage::new(message);
        record.input = Some(self.queued[index].clone());
        if let In::UserText { id: Some(id), .. } = &self.queued[index] {
            record.id = id.clone();
        }
        if initial && self.config.memory_search_limit > 0 && !text.trim().is_empty() {
            if let Some(memory) = self.tc.snap.memory.clone() {
                let cancel = self.tc.cancel.clone();
                let query = RecallRequest {
                    query: &text,
                    limit: self.config.memory_search_limit,
                    max_chars: self.config.memory_max_chars,
                };
                match self
                    .wait_operation(memory.recall(query, &cancel), self.op_timeout(), "memory")
                    .await
                {
                    Ok(entries) => {
                        let mut seen = std::collections::HashSet::new();
                        let mut selected = vec![];
                        for entry in entries {
                            if selected.len() >= self.config.memory_search_limit {
                                break;
                            }
                            if entry.content.trim().is_empty()
                                || !seen.insert((
                                    entry.provider.clone(),
                                    entry.id.clone(),
                                    entry.content.clone(),
                                ))
                            {
                                continue;
                            }
                            let mut candidate = selected.clone();
                            candidate.push(entry);
                            // Bound the rendered metadata too, not just provider prose.
                            if serde_json::to_string(&candidate)
                                .map_or(usize::MAX, |s| s.chars().count())
                                > self.config.memory_max_chars
                            {
                                continue;
                            }
                            selected = candidate;
                        }
                        record.attach_recall(selected);
                    }
                    Err(e @ YourAiError::Aborted(_)) => return Err(e),
                    Err(e) => {
                        self.notice(Level::Warning, format!("Memory recall unavailable: {e}"))?
                    }
                }
            }
        }
        if initial && !self.config.skill_ids.is_empty() {
            let skills =
                self.tc.snap.skills.clone().ok_or_else(|| {
                    ErrorKind::Config("selected skills require SkillProvider".into())
                })?;
            let mut parts = record
                .api_content
                .clone()
                .unwrap_or_else(|| record.message.content.clone())
                .into_parts();
            for id in &self.config.skill_ids.clone() {
                let skill = self
                    .wait_operation(skills.load(id), self.op_timeout(), "skill")
                    .await?;
                parts.push(ContentPart::from_text(format!(
                    "<skill id={id:?}>\n{}\n</skill>",
                    skill.instructions
                )));
            }
            record.api_content = Some(parts.into());
        }
        let mut records = vec![record];
        if !contexts.is_empty() {
            let mut context = StoredMessage::runtime_context(format!(
                "[Runtime context]\n{}",
                contexts.join("\n")
            ));
            context.id = format!("{}:admission-context", records[0].id);
            records.push(context);
        }
        self.wait_operation(history.append(records), self.op_timeout(), "history")
            .await?;
        self.queued.remove(index); // Transfer only after a successful commit.
        self.repeated_tool = None;
        Ok(true)
    }
    fn reject_input(&mut self, index: usize, reason: String) -> Result<bool, YourAiError> {
        // Publish before removing: a disconnected consumer must not lose
        // the original input. Reported rejections never enter pending.
        let rejection = InputRejected {
            input: self.queued[index].clone(),
            reason,
        };
        self.send(Out::InputRejected {
            rejection: rejection.clone(),
        })?;
        self.queued.remove(index);
        self.output.rejected.push(rejection);
        Ok(false)
    }
}
