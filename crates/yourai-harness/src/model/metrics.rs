use super::{usage, ModelBudget};
use serde::{Deserialize, Serialize};
use std::{sync::Arc, time::Instant};
use yourai_core::prelude::*;

/// Counters for this shared budget lifetime; no network request is made to collect them.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RequestMetrics {
    pub active: u64,
    #[serde(default)]
    pub cooldown_seconds: u64,
    #[serde(default)]
    pub journal_errors: u64,
    pub attempts_last_minute: u64,
    pub completed: u64,
    pub failed: u64,
    pub cancelled: u64,
    pub rate_limited: u64,
    /// Latest completed response: output tokens / request-to-End elapsed seconds (includes TTFT).
    pub last_output_tokens_per_second: Option<f64>,
    pub cache_read_tokens: u64,
    /// Only inputs whose response reports both prompt and cache-read counters.
    pub cache_known_input_tokens: u64,
    pub cache_reported_responses: u64,
}
impl RequestMetrics {
    pub fn cache_hit_percent(&self) -> Option<f64> {
        (self.cache_known_input_tokens > 0)
            .then(|| self.cache_read_tokens as f64 * 100.0 / self.cache_known_input_tokens as f64)
    }
}
pub(super) struct Attempt {
    budget: Arc<ModelBudget>,
    start: Instant,
    done: bool,
    request_id: Option<String>,
}
impl Attempt {
    pub fn new(budget: Arc<ModelBudget>) -> Self {
        Self {
            budget,
            start: Instant::now(),
            done: false,
            request_id: None,
        }
    }
    pub fn start(budget: Arc<ModelBudget>, request: &ModelRequest, model: &str) -> Self {
        let mut attempt = Self::new(budget);
        match attempt.budget.control.start(request, model) {
            Ok(id) => attempt.request_id = id,
            Err(_) => attempt.budget.state.lock().unwrap().requests.journal_errors += 1,
        }
        attempt
    }
    fn record(&self, outcome: &str, status: Option<u16>) {
        if self
            .budget
            .control
            .finish(
                self.request_id.as_deref(),
                outcome,
                status,
                self.start.elapsed(),
            )
            .is_err()
        {
            self.budget.state.lock().unwrap().requests.journal_errors += 1;
        }
    }
    pub fn finish(&mut self, raw: Option<&GenaiUsage>) {
        if self.done {
            return;
        }
        self.done = true;
        self.record("completed", None);
        let mut state = self.budget.state.lock().unwrap();
        if let Some(raw) = raw {
            let u = usage(raw);
            state.usage.input_tokens = state.usage.input_tokens.saturating_add(u.input_tokens);
            state.usage.output_tokens = state.usage.output_tokens.saturating_add(u.output_tokens);
            state.usage.total_tokens = state.usage.total_tokens.saturating_add(u.total_tokens);
        }
        let metrics = &mut state.requests;
        metrics.active = metrics.active.saturating_sub(1);
        metrics.completed += 1;
        metrics.last_output_tokens_per_second = raw
            .and_then(|u| u.completion_tokens)
            .filter(|n| *n >= 0)
            .map(|n| n as f64 / self.start.elapsed().as_secs_f64().max(0.001));
        if let Some(raw) = raw {
            if let (Some(input), Some(cached)) = (
                raw.prompt_tokens,
                raw.prompt_tokens_details
                    .as_ref()
                    .and_then(|d| d.cached_tokens),
            ) {
                if input >= 0 && cached >= 0 && cached <= input {
                    metrics.cache_known_input_tokens += input as u64;
                    metrics.cache_read_tokens += cached as u64;
                    metrics.cache_reported_responses += 1;
                }
            }
        }
    }
    pub fn fail(&mut self, error: Option<&YourAiError>) {
        if self.done {
            return;
        }
        self.done = true;
        let status = error
            .and_then(YourAiError::model_http_error)
            .map(|(status, _)| status);
        self.record("failed", status);
        if status == Some(429) && error.is_some_and(super::control::transient_limit) {
            self.budget
                .control
                .cool_down(error.and_then(super::control::retry_after));
        }
        let mut state = self.budget.state.lock().unwrap();
        state.requests.active = state.requests.active.saturating_sub(1);
        state.requests.failed += 1;
        if error
            .and_then(YourAiError::model_http_error)
            .is_some_and(|(status, _)| status == 429)
        {
            state.requests.rate_limited += 1;
        }
    }
}
impl Drop for Attempt {
    fn drop(&mut self) {
        if !self.done {
            self.record("cancelled", None);
            let mut state = self.budget.state.lock().unwrap();
            state.requests.active = state.requests.active.saturating_sub(1);
            state.requests.cancelled += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn http_error(body: &str) -> YourAiError {
        let error = genai::Error::HttpError {
            status: "429".parse().unwrap(),
            canonical_reason: "Too Many Requests".into(),
            body: body.into(),
        };
        ErrorKind::Model {
            source: genai::Error::WebStream {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm"),
                cause: error.to_string(),
                error: Box::new(error),
            },
        }
        .into()
    }
    #[test]
    fn counts_failures_cancellation_and_known_usage_exactly_once() {
        let budget = ModelBudget::new(None, None);
        budget.reserve().unwrap();
        let mut attempt = Attempt::new(budget.clone());
        attempt.fail(Some(&http_error("{}")));
        drop(attempt);
        budget.reserve().unwrap();
        drop(Attempt::new(budget.clone()));
        budget.reserve().unwrap();
        let mut attempt = Attempt::new(budget.clone());
        let raw = GenaiUsage {
            prompt_tokens: Some(100),
            completion_tokens: Some(20),
            prompt_tokens_details: Some(genai::chat::PromptTokensDetails {
                cached_tokens: Some(80),
                ..Default::default()
            }),
            ..Default::default()
        };
        attempt.finish(Some(&raw));
        attempt.finish(Some(&raw));
        drop(attempt);
        let s = budget.snapshot();
        assert_eq!(
            (
                s.calls,
                s.requests.active,
                s.requests.failed,
                s.requests.cancelled,
                s.requests.completed,
                s.requests.rate_limited
            ),
            (3, 0, 1, 1, 1, 1)
        );
        assert_eq!(s.requests.attempts_last_minute, 3);
        assert_eq!(s.usage.total_tokens, 120);
        assert_eq!(s.requests.cache_hit_percent(), Some(80.0));
        assert!(s
            .requests
            .last_output_tokens_per_second
            .unwrap()
            .is_finite());
        budget.reserve().unwrap();
        Attempt::new(budget.clone()).finish(None);
        assert!(budget
            .snapshot()
            .requests
            .last_output_tokens_per_second
            .is_none());
        assert_eq!(budget.snapshot().requests.cache_reported_responses, 1);
    }
    #[tokio::test(start_paused = true)]
    async fn transient_429_without_retry_after_uses_configured_cooldown() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::SqliteStore::open(&dir.path().join("sessions.sqlite3")).unwrap();
        let budget = ModelBudget::configured(
            None,
            None,
            super::super::RequestPolicy {
                rpm: None,
                cooldown_seconds: 7,
            },
            store,
        )
        .unwrap();
        budget.reserve().unwrap();
        Attempt::new(budget.clone()).fail(Some(&http_error(
            r#"{"error":{"code":"rate_limit_exceeded"}}"#,
        )));
        assert_eq!(
            budget.control.remaining(),
            std::time::Duration::from_secs(7)
        );
        tokio::time::advance(std::time::Duration::from_secs(7)).await;
        assert!(budget.control.remaining().is_zero());
    }

    #[test]
    fn quota_429_is_not_treated_as_transient_rate_limit() {
        struct Provider;
        impl ModelProvider for Provider {
            fn model_iden(&self) -> &str {
                "test"
            }
            fn complete<'a>(
                &'a self,
                _: ModelRequest,
            ) -> BoxFuture<'a, Result<ChatResponse, YourAiError>> {
                Box::pin(async { unreachable!() })
            }
            fn stream_events<'a>(
                &'a self,
                _: ModelRequest,
            ) -> BoxFuture<'a, Result<ModelEventStream, YourAiError>> {
                Box::pin(async { unreachable!() })
            }
        }
        assert_eq!(
            Provider.recovery(&http_error(r#"{"error":{"code":"insufficient_quota"}}"#)),
            ModelRecovery::Fatal
        );
        assert_eq!(
            Provider.recovery(&http_error(r#"{"error":{"code":"rate_limit_exceeded"}}"#)),
            ModelRecovery::Retry
        );
    }
}
