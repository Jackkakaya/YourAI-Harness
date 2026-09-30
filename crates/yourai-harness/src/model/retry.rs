//! Shared model retry timing: server hints, compatibility backoff and safe delay bounds.
//! Request admission and the loop use the same interpretation of server timing.
use std::time::Duration;
use yourai_core::prelude::*;

pub(super) const MAX_DELAY: Duration = Duration::from_millis(i32::MAX as u64);

/// OpenCode compatibility policy: header-bearing failures use the global safety
/// bound; failures without headers also use the configured local backoff cap.
/// Kept here so callers do not branch on protocol metadata.
pub(crate) struct Backoff {
    pub initial: Duration,
    pub max_without_headers: Duration,
}
impl Backoff {
    pub(crate) fn delay(
        &self,
        retries: u32,
        error: &YourAiError,
        hint: Option<Duration>,
        jitter_percent: u32,
    ) -> Duration {
        if let Some(hint) = hint {
            return hint.min(MAX_DELAY);
        }
        let base = self
            .initial
            .saturating_mul(1u32 << retries.min(31))
            .min(MAX_DELAY);
        let delay = base.saturating_add(base / 4 * jitter_percent.min(100) / 100);
        if error.model_has_http_headers() {
            delay.min(MAX_DELAY)
        } else {
            delay.min(self.max_without_headers).min(MAX_DELAY)
        }
    }
}

/// Preserve server timing when the SDK exposes it; do not guess vendor-specific body units.
pub(super) fn retry_after(error: &YourAiError) -> Option<Duration> {
    // Accept fractional server values and cap before Duration/Instant arithmetic.
    fn duration(value: &str, scale: f64) -> Option<Duration> {
        let value = value.trim().parse::<f64>().ok()?;
        if !value.is_finite() || value < 0.0 {
            return None;
        }
        Some(Duration::from_millis(
            (value * scale).ceil().min(MAX_DELAY.as_millis() as f64) as u64,
        ))
    }
    if let Some(ms) = error
        .model_http_header("retry-after-ms")
        .and_then(|v| duration(v, 1.0))
    {
        return Some(ms);
    }
    let value = error.model_http_header("retry-after")?.trim();
    if let Some(seconds) = duration(value, 1000.0) {
        return Some(seconds);
    }
    let date = httpdate::parse_http_date(value).ok()?;
    date.duration_since(std::time::SystemTime::now())
        .ok()
        .map(|d| d.min(MAX_DELAY))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn http_error(header: &str, value: &str) -> YourAiError {
        http_headers(&[(header, value)])
    }
    fn http_headers(values: &[(&str, &str)]) -> YourAiError {
        let mut headers = reqwest::header::HeaderMap::new();
        for (header, value) in values {
            headers.insert(
                header.parse::<reqwest::header::HeaderName>().unwrap(),
                value.parse().unwrap(),
            );
        }
        ErrorKind::Model {
            source: genai::Error::WebModelCall {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test"),
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status: "429".parse().unwrap(),
                    body: "{}".into(),
                    headers: Box::new(headers),
                },
            },
        }
        .into()
    }
    #[test]
    fn nonstream_status_and_retry_after_headers_survive_sdk_errors() {
        let error = http_error("retry-after-ms", "1500");
        assert_eq!(error.model_http_error(), Some((429, "{}")));
        assert_eq!(retry_after(&error), Some(Duration::from_millis(1500)));
        assert_eq!(
            retry_after(&http_error("retry-after", "120")),
            Some(Duration::from_secs(120))
        );
        let future =
            httpdate::fmt_http_date(std::time::SystemTime::now() + Duration::from_secs(120));
        let delay = retry_after(&http_error("retry-after", &future)).unwrap();
        assert!(delay.as_secs() >= 118 && delay.as_secs() <= 120);
        assert!(retry_after(&http_error("retry-after", "invalid")).is_none());
        assert_eq!(
            retry_after(&http_error("retry-after", "0.25")),
            Some(Duration::from_millis(250))
        );
        assert_eq!(
            retry_after(&http_error("retry-after-ms", "1.5")),
            Some(Duration::from_millis(2))
        );
        assert_eq!(
            retry_after(&http_error("retry-after", "1e100")),
            Some(Duration::from_millis(i32::MAX as u64))
        );
        assert!(retry_after(&http_error("retry-after", "-1")).is_none());
    }

    fn error(headers: bool) -> YourAiError {
        let source = if headers {
            genai::Error::WebModelCall {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "test"),
                webc_error: genai::webc::Error::ResponseFailedStatus {
                    status: "429".parse().unwrap(),
                    body: "{}".into(),
                    headers: Box::default(),
                },
            }
        } else {
            genai::Error::HttpError {
                status: "429".parse().unwrap(),
                canonical_reason: "Too Many Requests".into(),
                body: "{}".into(),
            }
        };
        ErrorKind::Model { source }.into()
    }

    #[test]
    fn backoff_jitter_headers_and_server_hints() {
        let backoff = Backoff {
            initial: Duration::from_secs(2),
            max_without_headers: Duration::from_secs(30),
        };
        let bare = error(false);
        assert_eq!(backoff.delay(0, &bare, None, 0), Duration::from_secs(2));
        assert_eq!(backoff.delay(1, &bare, None, 100), Duration::from_secs(5));
        assert_eq!(backoff.delay(4, &bare, None, 0), Duration::from_secs(30));
        assert_eq!(
            backoff.delay(4, &error(true), None, 0),
            Duration::from_secs(32)
        );
        assert_eq!(
            backoff.delay(4, &bare, Some(Duration::from_millis(100)), 0),
            Duration::from_millis(100)
        );
        assert_eq!(backoff.delay(0, &bare, Some(Duration::MAX), 0), MAX_DELAY);
    }
    #[test]
    fn server_header_precedence_and_zero_are_preserved() {
        for (milliseconds, seconds, expected) in [
            ("0", "12", Some(Duration::ZERO)),
            ("1500", "12", Some(Duration::from_millis(1500))),
            ("invalid", "0.25", Some(Duration::from_millis(250))),
            ("NaN", "12", Some(Duration::from_secs(12))),
            ("-1", "inf", None),
            ("invalid", "Thu, 01 Jan 1970 00:00:00 GMT", None),
        ] {
            let error = http_headers(&[("retry-after-ms", milliseconds), ("retry-after", seconds)]);
            assert_eq!(retry_after(&error), expected);
        }
        let backoff = Backoff {
            initial: Duration::MAX,
            max_without_headers: Duration::MAX,
        };
        assert_eq!(
            backoff.delay(u32::MAX, &error(false), Some(Duration::ZERO), 100),
            Duration::ZERO
        );
        assert_eq!(
            backoff.delay(u32::MAX, &error(false), None, u32::MAX),
            MAX_DELAY
        );
    }

    #[tokio::test(start_paused = true)]
    async fn admission_and_retry_share_server_delay_and_safety_bound() {
        for (value, expected) in [
            ("0", Duration::ZERO),
            ("0.25", Duration::from_millis(250)),
            ("1e100", MAX_DELAY),
        ] {
            let budget = super::super::ModelBudget::configured(
                super::super::RequestPolicy {
                    rpm: None,
                    cooldown_seconds: 7,
                },
                crate::SqliteStore::open(std::path::Path::new(":memory:")).unwrap(),
            )
            .unwrap();
            let error = http_error("retry-after", value);
            budget
                .control
                .on_failure(ModelErrorClass::RateLimited, retry_after(&error));
            let backoff = Backoff {
                initial: Duration::from_secs(2),
                max_without_headers: Duration::from_secs(30),
            };
            assert_eq!(budget.control.remaining(), expected);
            assert_eq!(backoff.delay(0, &error, retry_after(&error), 100), expected);
        }
    }

    #[tokio::test(start_paused = true)]
    async fn metered_retry_preserves_custom_provider_policy() {
        struct HintProvider(Option<Duration>);
        impl ModelProvider for HintProvider {
            fn model_iden(&self) -> &str {
                "custom"
            }
            fn retry_after(&self, _: &YourAiError) -> Option<Duration> {
                self.0
            }
            fn recovery(&self, _: &YourAiError) -> ModelRecovery {
                ModelRecovery::Compact
            }
            fn complete<'a>(
                &'a self,
                _: ModelRequest,
            ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
                unreachable!()
            }
            fn stream_events<'a>(
                &'a self,
                _: ModelRequest,
            ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
                unreachable!()
            }
        }
        for hint in [
            None,
            Some(Duration::ZERO),
            Some(Duration::from_secs(2)),
            Some(Duration::from_secs(10)),
        ] {
            let budget = super::super::ModelBudget::new();
            let model = super::super::MeteredModel {
                inner: std::sync::Arc::new(HintProvider(hint)),
                budget: budget.clone(),
            };
            let error = http_error("retry-after", "7");
            // A provider hint is local policy; merely asking for it must not cool down peers.
            assert_eq!(model.retry_after(&error), hint);
            assert!(budget.control.remaining().is_zero());
            assert_eq!(model.recovery(&error), ModelRecovery::Compact);
            budget
                .control
                .on_failure(ModelErrorClass::RateLimited, retry_after(&error));
            assert_eq!(
                model.retry_after(&error),
                Some(hint.unwrap_or_default().max(Duration::from_secs(7)))
            );
            tokio::time::advance(Duration::from_secs(7)).await;
            assert_eq!(model.retry_after(&error), hint);
        }
    }
}
