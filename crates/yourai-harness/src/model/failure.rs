//! genai/OpenAI-compatible error interpretation; provider policy stays outside core.
use yourai_core::prelude::*;
/// Normalize known protocol codes before falling back to HTTP status.
/// A quota or context error is not a temporary provider throttle, even on 429.
/// Only structured fields are considered, never error display text.
pub(super) fn classify(error: &YourAiError) -> ModelErrorClass {
    let Some((status, body)) = error.model_http_error() else {
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
