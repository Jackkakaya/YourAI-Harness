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
                parts.push(attachment_to_part(att, &self.config.attachment_image)?);
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

// ── attachment normalization & genai Binary conversion ────────────────
//
// Mirrors opencode `packages/opencode/src/image/image.ts`:
// - images are normalized before entering history: within-limit images pass
//   through untouched, over-limit images are resized (Lanczos3; PNG first,
//   then JPEG at qualities 80/85/70/55/40; dimensions shrink ×0.75 per
//   ladder step, at most 32 candidates) and only an image that cannot be
//   brought within limits fails;
// - audio and PDF sizes are provider-side concerns and pass through.

use base64::{
    engine::{
        general_purpose::{GeneralPurpose, GeneralPurposeConfig},
        DecodePaddingMode,
    },
    Engine as _,
};

/// Standard alphabet, tolerant of missing padding on decode (frontends may
/// emit unpadded base64); always encodes with padding.
static BASE64: GeneralPurpose = GeneralPurpose::new(
    &base64::alphabet::STANDARD,
    GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
);

/// opencode JPEG quality ladder (80 deliberately before 85).
const JPEG_QUALITIES: [u8; 5] = [80, 85, 70, 55, 40];

/// How genai handles Binary per provider
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
fn attachment_to_part(
    att: &UserAttachment,
    cfg: &super::AttachmentImageConfig,
) -> Result<ContentPart, YourAiError> {
    let ct = att.content_type.trim().to_ascii_lowercase();
    if !(ct.starts_with("image/") || ct.starts_with("audio/") || ct == "application/pdf") {
        return Err(ErrorKind::Config(format!(
            "unsupported attachment type '{ct}'; only image/*, audio/*, and application/pdf are accepted"
        ))
        .into());
    }
    let (mime, data) = if ct.starts_with("image/") {
        normalize_image(att, &ct, cfg)?
    } else {
        (ct, att.data.clone())
    };
    Ok(ContentPart::from_binary_base64(
        mime,
        data.as_str(),
        att.name.clone(),
    ))
}

fn size_error(
    width: u32,
    height: u32,
    bytes: usize,
    cfg: &super::AttachmentImageConfig,
) -> YourAiError {
    ErrorKind::Config(format!(
        "Image {width}x{height} with base64 size {bytes} exceeds configured limits and could not be resized below {}x{}/{} bytes",
        cfg.max_width, cfg.max_height, cfg.max_base64_bytes
    ))
    .into()
}

/// Decode, size-check and (if needed) resize one image attachment.
/// Returns the effective MIME type and base64 payload.
fn normalize_image(
    att: &UserAttachment,
    ct: &str,
    cfg: &super::AttachmentImageConfig,
) -> Result<(String, String), YourAiError> {
    let bytes = BASE64
        .decode(att.data.as_bytes())
        .map_err(|_| ErrorKind::Config("attachment image is not valid base64".into()))?;
    let image = image::load_from_memory(&bytes)
        .map_err(|_| ErrorKind::Config("attachment image could not be decoded".into()))?;
    let (width, height) = (image.width(), image.height());
    if width <= cfg.max_width
        && height <= cfg.max_height
        && att.data.len() <= cfg.max_base64_bytes
    {
        // Fast path: within limits, pass the original payload through
        // untouched (no re-encode, no quality loss).
        return Ok((ct.to_owned(), att.data.clone()));
    }
    if !cfg.auto_resize {
        return Err(size_error(width, height, att.data.len(), cfg));
    }
    let scale = 1f64
        .min(cfg.max_width as f64 / width as f64)
        .min(cfg.max_height as f64 / height as f64);
    let mut size = (
        ((width as f64 * scale).round() as u32).max(1),
        ((height as f64 * scale).round() as u32).max(1),
    );
    // At most 32 candidate sizes, shrinking ×0.75 each step (opencode ladder).
    for _ in 0..32 {
        let resized = image.resize_exact(size.0, size.1, image::imageops::FilterType::Lanczos3);
        let mut candidates = vec![("image/png", encode_png(&resized))];
        for quality in JPEG_QUALITIES {
            candidates.push(("image/jpeg", encode_jpeg(&resized, quality)));
        }
        let encoded = candidates
            .into_iter()
            .map(|(mime, result)| result.map(|data| (mime, data)))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some((mime, data)) = encoded
            .into_iter()
            .find(|(_, data)| data.len() <= cfg.max_base64_bytes)
        {
            return Ok((mime.into(), data));
        }
        let next = (
            shrink_step(size.0),
            shrink_step(size.1),
        );
        if next == size {
            break;
        }
        size = next;
    }
    Err(size_error(width, height, att.data.len(), cfg))
}

fn shrink_step(dim: u32) -> u32 {
    if dim == 1 {
        1
    } else {
        ((dim as f64 * 0.75).floor() as u32).max(1)
    }
}

fn encode_png(image: &image::DynamicImage) -> Result<String, YourAiError> {
    let mut buf = Vec::new();
    image
        .to_rgb8()
        .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
        .map_err(|e| ErrorKind::Config(format!("png re-encode failed: {e}")))?;
    Ok(BASE64.encode(&buf))
}

fn encode_jpeg(image: &image::DynamicImage, quality: u8) -> Result<String, YourAiError> {
    let mut buf = Vec::new();
    let encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut buf, quality);
    image
        .to_rgb8()
        .write_with_encoder(encoder)
        .map_err(|e| ErrorKind::Config(format!("jpeg re-encode failed: {e}")))?;
    Ok(BASE64.encode(&buf))
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

    /// Blank in-memory image, base64-encoded.
    fn blank_png_base64(width: u32, height: u32) -> String {
        let img = image::DynamicImage::new_rgb8(width, height);
        encode_png(&img).unwrap()
    }

    /// Deterministic high-entropy image that resists PNG/JPEG compression.
    fn noisy_png_base64(width: u32, height: u32) -> String {
        let mut buf = image::RgbImage::new(width, height);
        for (x, y, pixel) in buf.enumerate_pixels_mut() {
            let v = ((x.wrapping_mul(31) as u64 + y.wrapping_mul(17) as u64
                + (x as u64) * (y as u64))
                % 256) as u8;
            *pixel = image::Rgb([v, v.wrapping_mul(3), v.wrapping_mul(7)]);
        }
        encode_png(&image::DynamicImage::from(buf)).unwrap()
    }

    fn limits() -> super::super::AttachmentImageConfig {
        super::super::AttachmentImageConfig {
            auto_resize: true,
            max_width: 64,
            max_height: 64,
            max_base64_bytes: 5 * 1024 * 1024,
        }
    }

    fn part_dims(part: &ContentPart) -> (u32, u32) {
        let binary = part.as_binary().unwrap();
        let BinarySource::Base64(data) = &binary.source else {
            panic!("expected base64 source");
        };
        let bytes = BASE64.decode(data.as_bytes()).unwrap();
        let img = image::load_from_memory(&bytes).unwrap();
        (img.width(), img.height())
    }

    #[test]
    fn image_attachment_within_limits_passes_through_unchanged() {
        let data = blank_png_base64(8, 8);
        let part = attachment_to_part(&att("image/png", &data), &limits()).unwrap();
        let binary = part.as_binary().unwrap();
        assert!(binary.is_image());
        assert_eq!(binary.content_type, "image/png");
        assert_eq!(binary.name.as_deref(), Some("test.bin"));
        // Fast path: base64 source is preserved verbatim, no re-encode.
        assert!(
            matches!(&binary.source, BinarySource::Base64(b) if b.as_ref() == data),
            "within-limit image must not be re-encoded"
        );
    }

    #[test]
    fn oversized_image_is_resized_by_default() {
        let part = attachment_to_part(&att("image/png", &noisy_png_base64(100, 100)), &limits())
            .unwrap();
        let (w, h) = part_dims(&part);
        assert!(w <= 64 && h <= 64, "resized to {w}x{h}");
        assert!(part.as_binary().unwrap().is_image());
    }

    #[test]
    fn byte_limit_drives_further_shrinking() {
        let mut cfg = limits();
        cfg.max_base64_bytes = 200; // forces the ladder well below 64x64
        let part = attachment_to_part(&att("image/png", &noisy_png_base64(200, 200)), &cfg).unwrap();
        let (w, h) = part_dims(&part);
        assert!(w <= 64 && h <= 64);
        let BinarySource::Base64(data) = &part.as_binary().unwrap().source else {
            panic!()
        };
        assert!(data.len() <= 200, "base64 len {} exceeds limit", data.len());
    }

    #[test]
    fn oversized_image_without_auto_resize_is_rejected() {
        let mut cfg = limits();
        cfg.auto_resize = false;
        let err =
            attachment_to_part(&att("image/png", &noisy_png_base64(100, 100)), &cfg).unwrap_err();
        assert!(err.to_string().contains("exceeds configured limits"));
    }

    #[test]
    fn invalid_image_data_is_rejected() {
        let err = attachment_to_part(&att("image/png", "aGVsbG8="), &limits()).unwrap_err();
        assert!(err.to_string().contains("could not be decoded"));
    }

    #[test]
    fn pdf_and_audio_pass_through_without_size_checks() {
        let pdf = attachment_to_part(&att("application/pdf", "JVBERi0="), &limits()).unwrap();
        assert!(pdf.as_binary().unwrap().is_pdf());

        let audio = attachment_to_part(&att("audio/wav", "UklGRiQ="), &limits()).unwrap();
        assert!(audio.as_binary().unwrap().is_audio());
    }

    #[test]
    fn unsupported_content_type_is_rejected() {
        let err = attachment_to_part(&att("text/plain", "aGVsbG8="), &limits()).unwrap_err();
        assert!(err.to_string().contains("unsupported attachment type"));
    }

    #[test]
    fn content_type_is_case_insensitive_and_trimmed() {
        let data = blank_png_base64(8, 8);
        let part = attachment_to_part(&att("  IMAGE/PNG  ", &data), &limits()).unwrap();
        assert_eq!(part.as_binary().unwrap().content_type, "image/png");
    }
}
