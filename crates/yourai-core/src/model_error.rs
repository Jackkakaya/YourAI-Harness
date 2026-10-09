//! genai/OpenAI-compatible error interpretation; provider policy stays outside core.
use crate::prelude::*;
use std::time::Duration;

/// One place understands the SDK's nested streaming/non-streaming errors.
fn error_leaf(error: &YourAiError) -> Option<&(dyn std::error::Error + 'static)> {
    let YourAiError::Error(ErrorKind::Model { source }) = error else {
        return None;
    };
    let mut current: &(dyn std::error::Error + 'static) = source;
    // Bounded walk: wrappers are finite, but a pathological chain must not hang here.
    for _ in 0..16 {
        match current.downcast_ref::<genai::Error>() {
            Some(genai::Error::WebStream { error, .. }) => current = error.as_ref(),
            Some(
                genai::Error::WebModelCall { webc_error, .. }
                | genai::Error::WebAdapterCall { webc_error, .. },
            ) => return Some(webc_error),
            _ => return Some(current),
        }
    }
    None
}

pub fn has_http_headers(error: &YourAiError) -> bool {
    let Some(error) = error_leaf(error) else {
        return false;
    };
    if let Some(genai::Error::HttpError { headers, .. }) = error.downcast_ref::<genai::Error>() {
        return !headers.is_empty();
    }
    matches!(error.downcast_ref::<genai::webc::Error>(), Some(genai::webc::Error::ResponseFailedStatus { headers, .. }) if !headers.is_empty())
}

pub fn http_header<'a>(error: &'a YourAiError, name: &str) -> Option<&'a str> {
    let error = error_leaf(error)?;
    if let Some(genai::Error::HttpError { headers, .. }) = error.downcast_ref::<genai::Error>() {
        return headers.get(name)?.to_str().ok();
    }
    match error.downcast_ref::<genai::webc::Error>()? {
        genai::webc::Error::ResponseFailedStatus { headers, .. } => {
            headers.get(name)?.to_str().ok()
        }
        _ => None,
    }
}

/// Typed status/body; never classify errors by their display text.
pub fn http_error(error: &YourAiError) -> Option<(u16, &str)> {
    let error = error_leaf(error)?;
    if let Some(genai::Error::HttpError { status, body, .. }) = error.downcast_ref::<genai::Error>()
    {
        return Some((status.as_u16(), body));
    }
    match error.downcast_ref::<genai::webc::Error>()? {
        genai::webc::Error::ResponseFailedStatus { status, body, .. } => {
            Some((status.as_u16(), body))
        }
        _ => None,
    }
}

/// Normalize known protocol codes before falling back to HTTP status.
/// A quota or context error is not a temporary provider throttle, even on 429.
/// Only structured fields are considered, never error display text.
pub fn classify(error: &YourAiError) -> ModelErrorClass {
    let Some((status, body)) = http_error(error) else {
        return ModelErrorClass::Unclassified;
    };
    let body = serde_json::from_str::<serde_json::Value>(body).unwrap_or_default();
    match body.pointer("/error/code").and_then(|code| code.as_str()) {
        Some("insufficient_quota" | "billing_hard_limit_reached") => {
            ModelErrorClass::QuotaExhausted
        }
        Some("context_length_exceeded") => ModelErrorClass::ContextOverflow,
        _ if status == 429 => ModelErrorClass::RateLimited,
        _ if (500..600).contains(&status) => ModelErrorClass::ServerError,
        _ => ModelErrorClass::Unclassified,
    }
}

pub const MAX_DELAY: Duration = Duration::from_millis(i32::MAX as u64);

/// OpenCode compatibility policy: header-bearing failures use the global safety
/// bound; failures without headers also use the configured local backoff cap.
/// Kept here so callers do not branch on protocol metadata.
pub struct Backoff {
    pub initial: Duration,
    pub max_without_headers: Duration,
}
impl Backoff {
    pub fn delay(
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
        if crate::model_error::has_http_headers(error) {
            delay.min(MAX_DELAY)
        } else {
            delay.min(self.max_without_headers).min(MAX_DELAY)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(error: genai::Error) -> genai::Error {
        genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm"),
            cause: error.to_string(),
            error: Box::new(error),
        }
    }

    #[test]
    fn http_status_survives_nested_stream_wrappers() {
        let source = genai::Error::HttpError {
            headers: Box::new(
                [("retry-after".parse().unwrap(), "3".parse().unwrap())]
                    .into_iter()
                    .collect(),
            ),
            status: "429".parse().unwrap(),
            canonical_reason: "Too Many Requests".into(),
            body: r#"{"error":{"message":"rpm exceeded","dimension":"rpm"}}"#.into(),
        };
        let error = YourAiError::from(ErrorKind::Model {
            source: wrap(wrap(source)),
        });
        let (status, body) = http_error(&error).unwrap();
        assert_eq!(status, 429);
        assert!(body.contains("rpm exceeded"));
        assert!(has_http_headers(&error));
        assert_eq!(http_header(&error, "retry-after"), Some("3"));
    }

    #[test]
    fn adapter_http_errors_share_status_headers_and_classification() {
        let source = genai::Error::WebAdapterCall {
            adapter_kind: genai::adapter::AdapterKind::OpenAI,
            webc_error: genai::webc::Error::ResponseFailedStatus {
                status: "429".parse().unwrap(),
                body: "{}".into(),
                headers: Box::new(
                    [("retry-after".parse().unwrap(), "2".parse().unwrap())]
                        .into_iter()
                        .collect(),
                ),
            },
        };
        let error = YourAiError::from(ErrorKind::Model {
            source: wrap(wrap(source)),
        });
        assert_eq!(http_error(&error), Some((429, "{}")));
        assert!(has_http_headers(&error));
        assert_eq!(http_header(&error, "retry-after"), Some("2"));
    }

    #[test]
    fn display_text_is_not_a_structured_http_status() {
        let error = YourAiError::from(ErrorKind::Model {
            source: genai::Error::WebStream {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm"),
                cause: "HTTP 429".into(),
                error: Box::new(std::io::Error::other("HTTP 429")),
            },
        });
        assert!(http_error(&error).is_none());
    }

    #[test]
    fn malformed_or_unrecognized_codes_fall_back_to_status() {
        for body in [
            "not JSON",
            "null",
            r#"{"error":{"code":42}}"#,
            r#"{"error":{"message":"insufficient_quota"}}"#,
            r#"{"error":{"code":"unknown_provider_code"}}"#,
        ] {
            for (status, expected) in [
                (400, ModelErrorClass::Unclassified),
                (429, ModelErrorClass::RateLimited),
                (503, ModelErrorClass::ServerError),
            ] {
                let error = YourAiError::from(ErrorKind::Model {
                    source: genai::Error::HttpError {
                        headers: Default::default(),
                        status: status.to_string().parse().unwrap(),
                        canonical_reason: String::new(),
                        body: body.into(),
                    },
                });
                assert_eq!(classify(&error), expected, "{status}: {body}");
            }
        }
    }
}
