//! User-input admission owns preparation, explicit rejection and history commit.
//! No input leaves the pending queue until either rejection is delivered or the
//! history write succeeds. Transient provider/storage failures retain ownership.
use super::{attachment, State};
use yourai_core::prelude::*;

impl State<'_> {
    pub(crate) async fn accept_input(
        &mut self,
        index: usize,
        initial: bool,
    ) -> Result<bool, YourAiError> {
        let (text, attachments) = match &self.queued[index] {
            In::UserText {
                text, attachments, ..
            } => (text.clone(), attachments.clone()),
            _ => return Ok(false),
        };
        let hook = self
            .hook(HookEvent::UserPromptSubmit {
                prompt: text.clone(),
            })
            .await?;
        self.apply_common(&hook)?;
        if !hook.common.blocking_errors.is_empty() {
            return self.reject_input(index, super::hooks::feedback(&hook).join("\n"));
        }
        let history = self.history.clone();
        let prepared = if attachments.is_empty() {
            Ok(ChatMessage::user(&text))
        } else {
            let cwd = self.tc.info.options.session.as_ref().map(|s| s.cwd.clone());
            let config = self.config.clone();
            let prompt = text.clone();
            // File I/O and image decoding must not block control handling.
            // A cancelled worker may finish its read, but cannot commit history.
            self.wait(
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
        if initial && self.config.memory_search_limit > 0 && !text.trim().is_empty() {
            if let Some(memory) = self.tc.snap.memory.clone() {
                let cancel = self.tc.cancel.clone();
                let query = RecallRequest {
                    query: &text,
                    limit: self.config.memory_search_limit,
                    max_chars: self.config.memory_max_chars,
                };
                match self
                    .wait(memory.recall(query, &cancel), self.op_timeout(), "memory")
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
                    .wait(skills.load(id), self.op_timeout(), "skill")
                    .await?;
                parts.push(ContentPart::from_text(format!(
                    "<skill id={id:?}>\n{}\n</skill>",
                    skill.instructions
                )));
            }
            record.api_content = Some(parts.into());
        }
        self.wait(history.append(vec![record]), self.op_timeout(), "history")
            .await?;
        self.queued.remove(index); // Transfer only after a successful commit.
        self.repeated_tool = None;
        self.add_context(&super::hooks::additional(&hook)).await?;
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
