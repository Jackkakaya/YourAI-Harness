//! OpenCode retry timing (session/retry.ts): server hints precede backoff.
use super::LoopConfig;
use std::time::Duration;
use yourai_core::prelude::*;

const MAX_DELAY: Duration = Duration::from_millis(i32::MAX as u64);

pub(super) fn delay(
    config: &LoopConfig,
    retries: u32,
    error: &YourAiError,
    hint: Option<Duration>,
    jitter_percent: u32,
) -> Duration {
    if let Some(hint) = hint {
        return hint.min(MAX_DELAY);
    }
    let base = config
        .retry_delay
        .saturating_mul(1u32 << retries.min(31))
        .min(MAX_DELAY);
    let delay = base.saturating_add(base / 4 * jitter_percent.min(100) / 100);
    if error.model_has_http_headers() {
        delay.min(MAX_DELAY)
    } else {
        delay.min(config.retry_max_delay).min(MAX_DELAY)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let config = LoopConfig::default();
        let bare = error(false);
        assert_eq!(delay(&config, 0, &bare, None, 0), Duration::from_secs(2));
        assert_eq!(delay(&config, 1, &bare, None, 100), Duration::from_secs(5));
        assert_eq!(delay(&config, 4, &bare, None, 0), Duration::from_secs(30));
        assert_eq!(
            delay(&config, 4, &error(true), None, 0),
            Duration::from_secs(32)
        );
        assert_eq!(
            delay(&config, 4, &bare, Some(Duration::from_millis(100)), 0),
            Duration::from_millis(100)
        );
        assert_eq!(delay(&config, 0, &bare, Some(Duration::MAX), 0), MAX_DELAY);
    }
}
