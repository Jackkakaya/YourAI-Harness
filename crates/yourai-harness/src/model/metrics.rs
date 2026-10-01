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
    pub fn fail(&mut self, error: Option<&YourAiError>) -> bool {
        if self.done {
            return false;
        }
        self.done = true;
        let status = error
            .and_then(super::failure::http_error)
            .map(|(status, _)| status);
        self.record("failed", status);
        let mut state = self.budget.state.lock().unwrap();
        state.requests.active = state.requests.active.saturating_sub(1);
        state.requests.failed += 1;
        if status == Some(429) {
            state.requests.rate_limited += 1;
        }
        true
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
        http_error_with_status(429, body)
    }
    fn http_error_with_status(status: u16, body: &str) -> YourAiError {
        let error = genai::Error::HttpError {
            status: status.to_string().parse().unwrap(),
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
        let budget = ModelBudget::new();
        budget.reserve();
        let mut attempt = Attempt::new(budget.clone());
        attempt.fail(Some(&http_error("{}")));
        drop(attempt);
        budget.reserve();
        drop(Attempt::new(budget.clone()));
        budget.reserve();
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
        budget.reserve();
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
            super::super::RequestPolicy {
                rpm: None,
                cooldown_seconds: 7,
            },
            store,
        )
        .unwrap();
        budget.reserve();
        let model = super::super::MeteredModel {
            inner: Arc::new(super::super::GenaiModel::new(
                genai::Client::default(),
                "test",
            )),
            budget: budget.clone(),
        };
        model.fail(
            &mut Attempt::new(budget.clone()),
            &http_error(r#"{"error":{"code":"rate_limit_exceeded"}}"#),
        );
        assert_eq!(
            budget.control.remaining(),
            std::time::Duration::from_secs(7)
        );
        tokio::time::advance(std::time::Duration::from_secs(7)).await;
        assert!(budget.control.remaining().is_zero());
    }

    #[tokio::test(start_paused = true)]
    async fn recovery_and_cooldown_agree_on_structured_error_classification() {
        let provider = Arc::new(super::super::GenaiModel::new(
            genai::Client::default(),
            "test",
        ));
        for (status, body, recovery, cooldown) in [
            (
                429,
                r#"{"error":{"code":"insufficient_quota"}}"#,
                ModelRecovery::Fatal,
                false,
            ),
            (
                429,
                r#"{"error":{"code":"billing_hard_limit_reached"}}"#,
                ModelRecovery::Fatal,
                false,
            ),
            (
                503,
                r#"{"error":{"code":"insufficient_quota"}}"#,
                ModelRecovery::Fatal,
                false,
            ),
            (
                429,
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                ModelRecovery::Compact,
                false,
            ),
            (
                400,
                r#"{"error":{"code":"context_length_exceeded"}}"#,
                ModelRecovery::Compact,
                false,
            ),
            (
                429,
                r#"{"error":{"code":"rate_limit_exceeded"}}"#,
                ModelRecovery::Retry,
                true,
            ),
            (429, "not JSON", ModelRecovery::Retry, true),
            (503, "not JSON", ModelRecovery::Retry, false),
            (401, "{}", ModelRecovery::Fatal, false),
        ] {
            let error = http_error_with_status(status, body);
            assert_eq!(provider.recovery(&error), recovery, "{status}: {body}");
            let budget = ModelBudget::configured(
                super::super::RequestPolicy {
                    rpm: None,
                    cooldown_seconds: 7,
                },
                crate::SqliteStore::open(std::path::Path::new(":memory:")).unwrap(),
            )
            .unwrap();
            budget.reserve();
            let model = super::super::MeteredModel {
                inner: provider.clone(),
                budget: budget.clone(),
            };
            model.fail(&mut Attempt::new(budget.clone()), &error);
            assert_eq!(
                !budget.control.remaining().is_zero(),
                cooldown,
                "{status}: {body}"
            );
            // This metric counts HTTP 429 responses, including permanent quota errors.
            assert_eq!(
                budget.snapshot().requests.rate_limited,
                u64::from(status == 429)
            );
            assert_eq!(budget.snapshot().requests.failed, 1);
        }
    }
}

#[cfg(test)]
mod journal_tests {
    use super::*;
    #[tokio::test(flavor = "current_thread")]
    async fn cancelled_attempts_do_not_block_and_flush_in_order_across_provider_switches() {
        let dir = tempfile::tempdir().unwrap();
        let store = crate::SqliteStore::open(&dir.path().join("log.sqlite3")).unwrap();
        let budget = ModelBudget::configured(Default::default(), store.clone()).unwrap();
        let other = budget.for_provider(Default::default()).unwrap();
        let (locked, acquired) = tokio::sync::oneshot::channel();
        let (release, waiting) = std::sync::mpsc::channel();
        let db = store.clone();
        let lock = std::thread::spawn(move || {
            db.with(|_| {
                locked.send(()).unwrap();
                waiting
                    .recv_timeout(std::time::Duration::from_secs(5))
                    .unwrap();
                Ok(())
            })
            .unwrap()
        });
        acquired.await.unwrap();
        let started = Instant::now();
        for budget in [budget.clone(), other] {
            budget.reserve();
            drop(Attempt::start(
                budget,
                &ModelRequest::new(ChatRequest::from_user("hello"), ChatOptions::default()),
                "test",
            ));
        }
        assert!(started.elapsed() < std::time::Duration::from_millis(100));
        assert!(
            tokio::time::timeout(std::time::Duration::from_millis(20), budget.flush())
                .await
                .is_err()
        );
        release.send(()).unwrap();
        budget.flush().await.unwrap();
        lock.join().unwrap();
        let rows: (i64, i64) = store
            .with(|db| {
                db.query_row(
                    "SELECT COUNT(*), SUM(outcome='cancelled') FROM model_requests",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .map_err(|e| crate::error("test", e))
            })
            .unwrap();
        assert_eq!(rows, (2, 2));
        assert_eq!(budget.snapshot().requests.cancelled, 2);
        assert_eq!(budget.snapshot().requests.journal_errors, 0);
    }
}
