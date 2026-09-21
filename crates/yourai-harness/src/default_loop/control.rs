use super::State;
use std::{
    future::Future,
    time::{Duration, Instant},
};
use yourai_core::prelude::*;

impl State<'_> {
    pub(crate) fn op_timeout(&self) -> Option<Duration> {
        Some(self.config.operation_timeout)
    }
    pub(crate) fn deadline(&self, timeout: Option<Duration>) -> Instant {
        let local = Instant::now() + timeout.unwrap_or(self.config.operation_timeout);
        self.tc
            .info
            .options
            .limits
            .deadline
            .map_or(local, |total| local.min(total))
    }
    pub(crate) fn timeout_error(&self, phase: &'static str) -> YourAiError {
        if self
            .tc
            .info
            .options
            .limits
            .deadline
            .is_some_and(|d| Instant::now() >= d)
        {
            AbortReason::DeadlineExceeded.into()
        } else {
            ErrorKind::Provider {
                name: phase,
                message: "operation timed out".into(),
            }
            .into()
        }
    }
    pub(crate) fn send(&self, out: Out) -> Result<(), YourAiError> {
        if self.tc.outbox.send(out) {
            Ok(())
        } else {
            Err(AbortReason::Disconnected.into())
        }
    }
    pub(crate) fn notice(
        &self,
        level: Level,
        message: impl Into<String>,
    ) -> Result<(), YourAiError> {
        self.send(Out::Notice {
            level,
            message: message.into(),
        })
    }
    pub(crate) fn route(&mut self, input: In) {
        // Replies are valid only while a particular request is awaiting them.
        if matches!(input, In::UserText { .. }) {
            self.queued.push_back(input);
        }
    }
    pub(crate) fn drain(&mut self) {
        // Bound the checkpoint to the snapshot length: producers cannot starve work.
        for _ in 0..self.tc.inbox.len() {
            match self.tc.inbox.try_recv() {
                Ok(input) => self.route(input),
                Err(_) => break,
            }
        }
    }
    pub(crate) fn has_steer(&self) -> bool {
        self.queued.iter().any(|i| {
            matches!(
                i,
                In::UserText {
                    mode: InputMode::Steer,
                    ..
                }
            )
        })
    }
    pub(crate) async fn wait<T>(
        &mut self,
        future: impl Future<Output = Result<T, YourAiError>>,
        timeout: Option<Duration>,
        phase: &'static str,
    ) -> Result<T, YourAiError> {
        self.tc.check_control()?;
        let deadline = self.deadline(timeout);
        tokio::pin!(future);
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(self.timeout_error(phase)),
                result = &mut future => return result,
                input = self.tc.inbox.recv(), if !self.input_closed => match input {
                    Some(input) => self.route(input), None => self.input_closed = true,
                },
            }
        }
    }
    pub(crate) async fn consume_events(&mut self) -> Result<(), YourAiError> {
        let Some(events) = self.tc.info.options.events.clone() else {
            return Ok(());
        };
        let count = events.pending().len();
        for _ in 0..count {
            let Some(event) = events.front() else { break };
            if let Some(context) = &event.context {
                let marker = format!("[Runtime event {}]", event.id);
                if !self.history.contains_context_marker(&marker) {
                    let history = self.history.clone();
                    self.wait(
                        history.append(vec![StoredMessage::runtime_context(format!(
                            "{marker}\n{context}"
                        ))]),
                        self.op_timeout(),
                        "history",
                    )
                    .await?;
                }
            }
            if let Some(notice) = &event.notice {
                self.notice(Level::Info, notice)?;
            }
            events.ack(&event.id);
        }
        Ok(())
    }
    pub(crate) fn has_events(&self) -> bool {
        self.tc
            .info
            .options
            .events
            .as_ref()
            .is_some_and(|e| e.has_context())
    }
    pub(crate) async fn checkpoint(&mut self) -> Result<(), YourAiError> {
        self.consume_events().await?;
        self.tc.check_control()?;
        self.drain();
        let contexts = std::mem::take(&mut self.deferred_context);
        self.add_context(&contexts).await?;
        // New arrivals during hooks are deferred to the next checkpoint.
        let mut remaining = self.queued.len();
        let mut index = 0;
        while remaining > 0 {
            remaining -= 1;
            if matches!(
                self.queued.get(index),
                Some(In::UserText {
                    mode: InputMode::Steer,
                    ..
                })
            ) {
                self.accept_input(index, false).await?;
            } else {
                index += 1;
            }
        }
        Ok(())
    }
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
            self.notice(Level::Warning, super::hooks::feedback(&hook).join("\n"))?;
            self.queued.remove(index); // Explicit rejection, not an unprocessed input.
            return Ok(false);
        }
        let history = self.history.clone();
        // Build the user message. Attachments (images, PDFs, audio) are
        // validated and converted to genai ContentPart::Binary — see
        // `attachment_to_part` for the provider-specific handling.
        let message = if attachments.is_empty() {
            ChatMessage::user(&text)
        } else {
            let mut parts = vec![ContentPart::from_text(text.clone())];
            for att in &attachments {
                parts.push(attachment_to_part(att)?);
            }
            ChatMessage::user(MessageContent::from_parts(parts))
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
        self.add_context(&super::hooks::additional(&hook)).await?;
        Ok(true)
    }
    pub(crate) async fn add_context(&mut self, contexts: &[String]) -> Result<(), YourAiError> {
        if contexts.is_empty() {
            return Ok(());
        }
        let history = self.history.clone();
        self.wait(
            history.append(vec![StoredMessage::runtime_context(format!(
                "[Runtime context]\n{}",
                contexts.join("\n")
            ))]),
            self.op_timeout(),
            "history",
        )
        .await
    }
    pub(crate) async fn record_usage(&mut self, usage: Usage) -> Result<(), YourAiError> {
        let total = self.output.usage.get_or_insert_with(Usage::default);
        total.input_tokens = total.input_tokens.saturating_add(usage.input_tokens);
        total.output_tokens = total.output_tokens.saturating_add(usage.output_tokens);
        total.total_tokens = total.total_tokens.saturating_add(usage.total_tokens);
        // Update the return report first, so accounting failures retain known usage.
        self.send(Out::Usage { usage }) // Each event is a delta; TurnOutput is cumulative.
    }
    pub(crate) async fn cleanup(&mut self, cause: &YourAiError) {
        let history = self.history.clone();
        let outbox = self.tc.outbox;
        let unresolved = std::mem::take(&mut self.unresolved);
        let completed = self.tool_completion.take();
        let partial = self.partial_message.take();
        // One cleanup deadline for the entire batch, independent of cancelled Turn token.
        let cleanup = async {
            if let Some(partial) = partial {
                history.append(vec![StoredMessage::new(partial)]).await?;
            }
            for call in unresolved {
                let (output, is_error) = completed.as_ref().filter(|(c,_,_)| c.call_id == call.call_id)
                    .map(|(_,v,e)| (v.clone(),*e)).unwrap_or_else(||
                        (serde_json::json!({"error": cause.to_string(), "status": "interrupted_or_not_executed"}), true));
                history
                    .append(vec![super::tools::result_record(&call, &output, is_error)])
                    .await?;
                outbox.send(Out::ToolDone {
                    id: call.call_id,
                    name: call.fn_name,
                    output,
                    is_error,
                });
            }
            Ok::<_, YourAiError>(())
        };
        let failure = match tokio::time::timeout(self.config.cleanup_timeout, cleanup).await {
            Ok(Ok(())) => None,
            Ok(Err(e)) => Some(e.to_string()),
            Err(_) => Some("cleanup timed out".into()),
        };
        if let Some(message) = failure {
            outbox.send(Out::Notice {
                level: Level::Error,
                message: format!(
                    "History cleanup incomplete; do not replay tools automatically: {message}"
                ),
            });
            if let Some(obs) = &self.tc.snap.observability {
                obs.increment("loop.cleanup_failed", &[]);
            }
        }
    }
}

// ── attachment validation & genai Binary conversion ──────────────────

/// Maximum decoded payload for a single attachment (20 MiB).
///
/// This is the most restrictive ceiling across the providers genai targets:
/// - OpenAI accepts images up to ~20 MB.
/// - Anthropic accepts base64 image data up to ~32 MB (~24 MB decoded).
///
/// 20 MiB decoded is safe for both.
const MAX_ATTACHMENT_BYTES: usize = 20 * 1024 * 1024;

/// Validate a [`UserAttachment`] and convert it to a genai
/// [`ContentPart::Binary`].
///
/// ## How genai handles Binary per provider
///
/// Binary parts are **only** processed in **User-role** messages; genai's
/// adapter code for assistant/tool roles silently ignores them. Since we
/// always emit `ChatMessage::user(...)`, this is correct.
///
/// genai's `Binary::is_image()` / `is_audio()` / `is_pdf()` classify by
/// `content_type` prefix, and each adapter translates accordingly:
///
/// | Provider   | image (base64)                    | audio (base64)              | PDF / other (base64)          |
/// |------------|-----------------------------------|-----------------------------|-------------------------------|
/// | OpenAI     | `image_url` + data URL            | `input_audio`               | `file` + file_data (data URL) |
/// | Anthropic  | `image` + base64 source           | `document` + base64 source  | `document` + base64 source    |
///
/// URL-sourced binaries have provider gaps (Anthropic can't do image URLs;
/// OpenAI can't do file URLs) — genai warns and skips those. We only emit
/// `BinarySource::Base64`, so both providers are covered.
///
/// ## Validation
///
/// 1. `content_type` must be `image/*`, `audio/*`, or `application/pdf` —
///    anything else is rejected with a clear error rather than silently
///    dropped by an adapter.
/// 2. The decoded payload (estimated from base64 length) must not exceed
///    [`MAX_ATTACHMENT_BYTES`].
fn attachment_to_part(att: &UserAttachment) -> Result<ContentPart, YourAiError> {
    let ct = att.content_type.trim().to_ascii_lowercase();
    if !(ct.starts_with("image/") || ct.starts_with("audio/") || ct == "application/pdf") {
        return Err(ErrorKind::Config(format!(
            "unsupported attachment type '{ct}'; only image/*, audio/*, and application/pdf are accepted"
        ))
        .into());
    }
    // Estimate decoded size from base64 length: 4 base64 chars ≈ 3 bytes.
    // This is a slight over-estimate when padding is present, which is fine
    // for a ceiling check.
    let estimated_bytes = att.data.len().saturating_mul(3) / 4;
    if estimated_bytes > MAX_ATTACHMENT_BYTES {
        return Err(ErrorKind::Config(format!(
            "attachment too large: ~{} MiB exceeds the {} MiB limit",
            estimated_bytes / (1024 * 1024),
            MAX_ATTACHMENT_BYTES / (1024 * 1024),
        ))
        .into());
    }
    Ok(ContentPart::from_binary_base64(
        ct,
        att.data.as_str(),
        att.name.clone(),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn att(content_type: &str, data: &str) -> UserAttachment {
        UserAttachment {
            content_type: content_type.into(),
            data: data.into(),
            name: Some("test.bin".into()),
        }
    }

    #[test]
    fn image_attachment_converts_to_binary_image() {
        let part = attachment_to_part(&att("image/png", "iVBORw0KGgo=")).unwrap();
        let binary = part.as_binary().unwrap();
        assert!(binary.is_image());
        assert_eq!(binary.content_type, "image/png");
        assert_eq!(binary.name.as_deref(), Some("test.bin"));
        // Base64 source is preserved verbatim.
        assert!(matches!(&binary.source, BinarySource::Base64(b) if b.as_ref() == "iVBORw0KGgo="));
    }

    #[test]
    fn pdf_and_audio_are_accepted() {
        let pdf = attachment_to_part(&att("application/pdf", "JVBERi0=")).unwrap();
        assert!(pdf.as_binary().unwrap().is_pdf());

        let audio = attachment_to_part(&att("audio/wav", "UklGRiQ=")).unwrap();
        assert!(audio.as_binary().unwrap().is_audio());
    }

    #[test]
    fn unsupported_content_type_is_rejected() {
        let err = attachment_to_part(&att("text/plain", "aGVsbG8=")).unwrap_err();
        assert!(err.to_string().contains("unsupported attachment type"));
    }

    #[test]
    fn oversized_attachment_is_rejected() {
        // base64 length L → decoded ≈ L*3/4 (integer division).  We need
        // estimated_bytes > MAX_ATTACHMENT_BYTES, so add enough margin to
        // clear the integer-division boundary.
        let big = "A".repeat(MAX_ATTACHMENT_BYTES * 4 / 3 + 100);
        let err = attachment_to_part(&att("image/png", &big)).unwrap_err();
        assert!(err.to_string().contains("too large"));
    }

    #[test]
    fn content_type_is_case_insensitive_and_trimmed() {
        let part = attachment_to_part(&att("  IMAGE/PNG  ", "iVBORw0KGgo=")).unwrap();
        assert_eq!(part.as_binary().unwrap().content_type, "image/png");
    }
}
