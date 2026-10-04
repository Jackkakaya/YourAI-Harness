//! genai/OpenAI-compatible error interpretation; provider policy stays outside core.
use yourai_core::prelude::*;

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
    matches!(
        error_leaf(error).and_then(|e| e.downcast_ref::<genai::webc::Error>()),
        Some(genai::webc::Error::ResponseFailedStatus { .. })
    )
}

pub fn http_header<'a>(error: &'a YourAiError, name: &str) -> Option<&'a str> {
    match error_leaf(error)?.downcast_ref::<genai::webc::Error>()? {
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
pub(super) fn classify(error: &YourAiError) -> ModelErrorClass {
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

/// User-facing diagnostic from typed SDK fields; no Display-string parsing.
pub fn diagnostic(error: &YourAiError) -> String {
    let Some((status, body)) = http_error(error) else {
        return error.to_string();
    };
    let body: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    let message = body
        .pointer("/error/message")
        .or_else(|| body.get("message"))
        .and_then(|v| v.as_str());
    match message {
        Some(message) => format!("Model request failed (HTTP {status}): {message}"),
        None => format!("Model request failed (HTTP {status})"),
    }
}

#[cfg(test)]
mod diagnostic_tests {
    use super::*;
    #[test]
    fn diagnostic_reads_typed_http_body_and_preserves_plain_errors() {
        for body in [
            r#"{"error":{"message":"unsupported image"}}"#,
            r#"{"message":"unsupported image"}"#,
        ] {
            let error = ErrorKind::Model {
                source: genai::Error::HttpError {
                    status: "400".parse().unwrap(),
                    canonical_reason: "Bad Request".into(),
                    body: body.into(),
                },
            }
            .into();
            assert_eq!(
                diagnostic(&error),
                "Model request failed (HTTP 400): unsupported image"
            );
        }
        let error = ErrorKind::Config("plain error".into()).into();
        assert_eq!(diagnostic(&error), error.to_string());
    }
}
