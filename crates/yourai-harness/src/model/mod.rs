mod control;
mod metrics;
pub use control::RequestPolicy;
use futures_util::StreamExt;
use metrics::Attempt;
pub use metrics::RequestMetrics;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use yourai_core::prelude::*;
pub fn usage(u: &GenaiUsage) -> Usage {
    let input = u.prompt_tokens.unwrap_or(0).max(0) as u64;
    let output = u.completion_tokens.unwrap_or(0).max(0) as u64;
    Usage {
        input_tokens: input,
        output_tokens: output,
        total_tokens: u
            .total_tokens
            .map(|n| n.max(0) as u64)
            .unwrap_or(input + output),
    }
}
/// Real network adapter. Client configuration controls endpoints and credentials.
pub struct GenaiModel {
    client: genai::Client,
    model: String,
    headers: genai::Headers,
}
impl GenaiModel {
    pub fn new(client: genai::Client, model: impl Into<String>) -> Self {
        Self {
            client,
            model: model.into(),
            headers: genai::Headers::default(),
        }
    }
    /// Defaults applied to every request, including compaction and child agents.
    pub fn with_headers(mut self, headers: genai::Headers) -> Self {
        self.headers = headers;
        self
    }
    fn request(&self, mut request: ModelRequest) -> ModelRequest {
        let mut headers = self.headers.clone();
        if let Some(overrides) = &request.options.extra_headers {
            headers.merge_with(overrides);
        }
        request.options.extra_headers = Some(headers);
        request
    }
}
impl ModelProvider for GenaiModel {
    fn retry_after(&self, error: &YourAiError) -> Option<std::time::Duration> {
        control::retry_after(error)
    }
    /// Media token estimation for the context pre-flight budget.
    ///
    /// opencode estimates everything by content length (`util/token.ts`,
    /// `length / 4`) because it has no pre-flight input gate — overflows are
    /// detected from provider errors and real usage. Our `MemoryContext`
    /// refuses to send a request whose estimate exceeds the input budget, so
    /// a base64-length estimate (a 5 MB image ≈ 1.25 M "tokens") would hard-
    /// fail every image turn. Instead we estimate what providers actually
    /// charge:
    ///
    /// - images: `max(anthropic (w·h)/750 after 1568 px downscale, openai
    ///   512 px tiles ×170 + 85)` from the decoded header dimensions;
    /// - PDF/audio: ~1 token per 16 decoded bytes (floor 2 000) — rough by
    ///   design;
    /// - URL-sourced binaries: flat 4 000 (unknown; never emitted by us).
    ///
    /// Deliberate over-estimates are safe only up to the point where the
    /// pre-flight gate false-fails, so formulas track provider pricing
    /// tables; the request-observation mechanism corrects drift with real
    /// usage on every response.
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        let binary = match part {
            ContentPart::Binary(binary) => binary,
            _ => {
                return Err(ErrorKind::Config(
                    "media_tokens expects a Binary part".into(),
                )
                .into())
            }
        };
        let base64_len = match &binary.source {
            BinarySource::Base64(data) => data.len() as u64,
            BinarySource::Url(_) => return Ok(4_000),
        };
        if !binary.is_image() {
            // PDF / audio: ~1 token per 16 decoded bytes, floored.
            return Ok((base64_len * 3 / 4 / 16).max(2_000));
        }
        let decoded = base64::Engine::decode(
            &base64::engine::general_purpose::STANDARD,
            match &binary.source {
                BinarySource::Base64(data) => data.as_bytes(),
                BinarySource::Url(_) => unreachable!("handled above"),
            },
        )
        .map_err(|_| ErrorKind::Config("attachment image is not valid base64".into()))?;
        // Header-only dimension read; fall back to the byte heuristic for
        // anything unparsable (e.g. foreign history records).
        let Some((width, height)) = image::ImageReader::new(std::io::Cursor::new(&decoded))
            .with_guessed_format()
            .ok()
            .and_then(|reader| reader.into_dimensions().ok())
            .map(|(w, h)| (w as u64, h as u64))
        else {
            return Ok((base64_len * 3 / 4 / 16).max(2_000));
        };
        Ok(image_media_tokens(width, height))
    }
    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        let r = self.request(r);
        Box::pin(async move {
            self.client
                .exec_chat(&self.model, r.request, Some(&r.options))
                .await
                .map_err(|source| ErrorKind::Model { source }.into())
        })
    }
    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        let r = self.request(r);
        Box::pin(async move {
            let response = self
                .client
                .exec_chat_stream(&self.model, r.request, Some(&r.options))
                .await
                .map_err(|source| YourAiError::from(ErrorKind::Model { source }))?;
            Ok(Box::pin(
                response
                    .stream
                    .map(|item| item.map_err(|source| ErrorKind::Model { source }.into())),
            ) as ModelEventStream)
        })
    }
    fn model_iden(&self) -> &str {
        &self.model
    }
}
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct BudgetSnapshot {
    #[serde(default)]
    pub requests: RequestMetrics,
    pub calls: u64,
    pub usage: Usage,
}
/// Shared by main model, compact, model hooks and subagents; admission is atomic.
pub struct ModelBudget {
    state: Mutex<BudgetSnapshot>,
    starts: Mutex<std::collections::VecDeque<std::time::Instant>>,
    control: control::RequestControl,
}
/// Provider-realistic image token estimate (see `GenaiModel::media_tokens`).
/// Integer arithmetic throughout: the estimate feeds a hard pre-flight gate
/// and must be deterministic across platforms.
fn image_media_tokens(width: u64, height: u64) -> u64 {
    let (width, height) = (width.max(1), height.max(1));
    let long = width.max(height);
    // Anthropic: scale the long edge to <= 1568 px, then (w*h)/750.
    let scaled = 1568.min(long);
    let (aw, ah) = (width * scaled / long, height * scaled / long);
    let anthropic = (aw * ah).div_ceil(750).max(1);
    // OpenAI high detail: fit within 2048^2, shortest side <= 768, then
    // 512^2 tiles at 170 tokens each plus 85 base tokens.
    let fit = 2048.min(long);
    let (mut w, mut h) = (width * fit / long, height * fit / long);
    let short = w.min(h).max(1);
    let side = 768.min(short);
    w = w * side / short;
    h = h * side / short;
    let tiles = w.div_ceil(512).saturating_mul(h.div_ceil(512));
    let openai = tiles.saturating_mul(170) + 85;
    anthropic.max(openai)
}

impl ModelBudget {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(BudgetSnapshot::default()),
            starts: Mutex::new(std::collections::VecDeque::new()),
            control: control::RequestControl::default(),
        })
    }
    pub fn configured(
        policy: RequestPolicy,
        store: crate::SqliteStore,
    ) -> Result<Arc<Self>, YourAiError> {
        policy.validate()?;
        Ok(Arc::new(Self {
            state: Mutex::new(BudgetSnapshot::default()),
            starts: Mutex::new(std::collections::VecDeque::new()),
            control: control::RequestControl::new(policy, Some(store)),
        }))
    }
    pub fn snapshot(&self) -> BudgetSnapshot {
        let mut snapshot = self.state.lock().unwrap().clone();
        let mut starts = self.starts.lock().unwrap();
        starts.retain(|t| t.elapsed() < std::time::Duration::from_secs(60));
        snapshot.requests.attempts_last_minute = starts.len() as u64;
        drop(starts);
        snapshot.requests.cooldown_seconds = self.control.remaining().as_secs_f64().ceil() as u64;
        snapshot
    }
    fn reserve(&self) {
        let mut s = self.state.lock().unwrap();
        s.calls += 1;
        s.requests.active += 1;
        let mut starts = self.starts.lock().unwrap();
        starts.retain(|t| t.elapsed() < std::time::Duration::from_secs(60));
        starts.push_back(std::time::Instant::now());
    }
}
pub struct MeteredModel {
    pub inner: Arc<dyn ModelProvider>,
    pub budget: Arc<ModelBudget>,
}
impl ModelProvider for MeteredModel {
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        self.inner.media_tokens(part)
    }

    fn complete<'a>(&'a self, r: ModelRequest) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        Box::pin(async move {
            self.budget.admit().await?;
            let mut attempt = Attempt::start(self.budget.clone(), &r, self.model_iden());
            match self.inner.complete(r).await {
                Ok(result) => {
                    attempt.finish(Some(&result.usage));
                    Ok(result)
                }
                Err(error) => {
                    attempt.fail(Some(&error));
                    Err(error)
                }
            }
        })
    }

    fn stream_events<'a>(
        &'a self,
        r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        Box::pin(async move {
            self.budget.admit().await?;
            let mut attempt = Attempt::start(self.budget.clone(), &r, self.model_iden());
            let stream = match self.inner.stream_events(r).await {
                Ok(stream) => stream,
                Err(error) => {
                    attempt.fail(Some(&error));
                    return Err(error);
                }
            };
            Ok(Box::pin(futures_util::stream::unfold(
                (stream, attempt),
                |(mut stream, mut attempt)| async move {
                    match stream.next().await {
                        Some(event) => {
                            match &event {
                                Ok(ChatStreamEvent::End(end)) => {
                                    attempt.finish(end.captured_usage.as_ref())
                                }
                                Err(error) => attempt.fail(Some(error)),
                                _ => {}
                            }
                            Some((event, (stream, attempt)))
                        }
                        None => {
                            attempt.fail(None);
                            None
                        }
                    }
                },
            )) as ModelEventStream)
        })
    }
    fn retry_after(&self, e: &YourAiError) -> Option<std::time::Duration> {
        Some(
            self.budget
                .control
                .remaining()
                .max(self.inner.retry_after(e).unwrap_or_default()),
        )
    }
    fn recovery(&self, e: &YourAiError) -> ModelRecovery {
        self.inner.recovery(e)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
}

/// Labels agentic hook calls too, while preserving their session IDs and shared admission.
pub(crate) struct SourceModel {
    pub inner: Arc<dyn ModelProvider>,
    pub source: &'static str,
}
impl ModelProvider for SourceModel {
    fn complete<'a>(
        &'a self,
        mut r: ModelRequest,
    ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
        r.source = self.source;
        self.inner.complete(r)
    }
    fn stream_events<'a>(
        &'a self,
        mut r: ModelRequest,
    ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
        r.source = self.source;
        self.inner.stream_events(r)
    }
    fn model_iden(&self) -> &str {
        self.inner.model_iden()
    }
    fn recovery(&self, e: &YourAiError) -> ModelRecovery {
        self.inner.recovery(e)
    }
    fn retry_after(&self, e: &YourAiError) -> Option<std::time::Duration> {
        self.inner.retry_after(e)
    }
    fn media_tokens(&self, part: &ContentPart) -> Result<u64, YourAiError> {
        self.inner.media_tokens(part)
    }
}

#[cfg(test)]
mod media_tokens_tests {
    use super::*;
    use base64::Engine as _;

    fn model() -> GenaiModel {
        GenaiModel::new(genai::Client::builder().build(), "test-model")
    }

    fn png_part(width: u32, height: u32) -> ContentPart {
        let img = image::DynamicImage::new_rgb8(width, height);
        let mut buf = Vec::new();
        img.to_rgb8()
            .write_to(&mut std::io::Cursor::new(&mut buf), image::ImageFormat::Png)
            .unwrap();
        let data = base64::engine::general_purpose::STANDARD.encode(&buf);
        ContentPart::from_binary_base64("image/png", data.as_str(), None)
    }

    #[test]
    fn image_estimate_tracks_provider_formulas() {
        // 2000x2000: anthropic 1568^2/750 = 3279; openai 768x768 tiles = 765.
        assert_eq!(image_media_tokens(2000, 2000), 3279);
        // 512x512: anthropic 512^2/750 = 350; openai single tile = 255.
        assert_eq!(image_media_tokens(512, 512), 350);
        // Tiny icon: anthropic 1; openai 255 -> 255.
        assert_eq!(image_media_tokens(64, 64), 255);
        // 4000x1000 wide banner: anthropic scales to 1568x392 -> 820;
        // openai 2048x512 -> 4 tiles = 765.
        assert_eq!(image_media_tokens(4000, 1000), 820);
    }

    #[test]
    fn binary_image_part_estimates_from_dimensions() {
        let model = model();
        let tokens = model.media_tokens(&png_part(2000, 2000)).unwrap();
        assert_eq!(tokens, 3279);
    }

    #[test]
    fn pdf_and_audio_estimate_by_bytes_with_floor() {
        let model = model();
        // ~1200 decoded bytes -> 75 -> floored to 2000.
        let small_pdf = ContentPart::from_binary_base64(
            "application/pdf",
            "A".repeat(1600).as_str(),
            None,
        );
        assert_eq!(model.media_tokens(&small_pdf).unwrap(), 2_000);
        // ~3 MB decoded -> 196 608.
        let big_pdf = ContentPart::from_binary_base64(
            "application/pdf",
            "A".repeat(4 * 1024 * 1024).as_str(),
            None,
        );
        assert_eq!(model.media_tokens(&big_pdf).unwrap(), 196_608);
    }

    #[test]
    fn url_sourced_binary_gets_flat_estimate() {
        let model = model();
        let part = ContentPart::Binary(Binary::from_url("image/png", "https://x/y.png", None));
        assert_eq!(model.media_tokens(&part).unwrap(), 4_000);
    }

    #[test]
    fn non_binary_part_stays_fail_closed() {
        let model = model();
        assert!(model.media_tokens(&ContentPart::from_text("hi")).is_err());
    }
}
