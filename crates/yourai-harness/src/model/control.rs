//! One admission/cooldown gate for every model sharing this Harness budget.
use super::ModelBudget;
use crate::storage::request_log::RequestLog;
#[cfg(test)]
use crate::SqliteStore;
use serde::{Deserialize, Serialize};
use std::{sync::Mutex, time::Duration};
use tokio::time::Instant;
use yourai_core::prelude::*;

#[derive(Debug, Clone, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct RequestPolicy {
    /// Optional known provider quota. Requests are paced evenly, without a burst allowance.
    pub rpm: Option<u32>,
    /// Opt-in shared fallback for HTTP 429 without a server delay; zero disables it.
    pub cooldown_seconds: u64,
}
impl RequestPolicy {
    pub fn validate(&self) -> Result<(), YourAiError> {
        if self.rpm.is_some_and(|n| n == 0 || n > 60_000) || self.cooldown_seconds > 3600 {
            return Err(ErrorKind::Config(
                "requests.rpm must be 1..60000; cooldown_seconds must be 0..3600".into(),
            )
            .into());
        }
        Ok(())
    }
}
pub(super) struct RequestControl {
    policy: RequestPolicy,
    deadlines: Mutex<(Instant, Instant)>, // next paced admission, shared cooldown
    pub(super) log: Option<std::sync::Arc<RequestLog>>,
    run_id: String,
}
impl Default for RequestControl {
    fn default() -> Self {
        Self::new(RequestPolicy::default(), None)
    }
}
impl RequestControl {
    pub fn new(policy: RequestPolicy, log: Option<std::sync::Arc<RequestLog>>) -> Self {
        Self {
            policy,
            deadlines: Mutex::new((Instant::now(), Instant::now())),
            log,
            run_id: uuid::Uuid::new_v4().to_string(),
        }
    }
    pub fn remaining(&self) -> Duration {
        self.deadlines
            .lock()
            .unwrap()
            .1
            .saturating_duration_since(Instant::now())
    }
    pub fn cool_down(&self, delay: Option<Duration>) {
        let mut deadlines = self.deadlines.lock().unwrap();
        let now = Instant::now();
        let delay = delay.unwrap_or(Duration::from_secs(self.policy.cooldown_seconds));
        // Untrusted headers must not panic on an overflowing Instant. Execution deadlines
        // will abort this wait; never turn an absurd server delay into an immediate retry.
        let until = now
            .checked_add(delay)
            .unwrap_or(now + Duration::from_secs(100 * 365 * 86400));
        deadlines.1 = deadlines.1.max(until);
    }
    pub fn start(
        &self,
        request: &ModelRequest,
        model: &str,
    ) -> Result<Option<String>, YourAiError> {
        let Some(log) = &self.log else {
            return Ok(None);
        };
        let id = uuid::Uuid::new_v4().to_string();
        let row_id = id.clone();
        let run = self.run_id.clone();
        let session = request.session_id.clone();
        let source = request.source;
        let model = model.to_owned();
        let turn = request.turn_id.clone();
        let attempt = request.attempt;
        let started = crate::storage::sqlite::now();
        log.write(move |store| store.with(|db| {
            db.execute("INSERT INTO model_requests(request_id,run_id,session_id,source,model,started_at,turn_id,attempt,outcome) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,'started')",
                rusqlite::params![row_id, run, session, source, model, started, turn, attempt]).map_err(|e| crate::error("request_log", e))?;
            Ok(())
        }))?;
        Ok(Some(id))
    }
    pub fn finish(
        &self,
        id: Option<&str>,
        outcome: &str,
        status: Option<u16>,
        elapsed: Duration,
    ) -> Result<(), YourAiError> {
        if let (Some(log), Some(id)) = (&self.log, id) {
            let id = id.to_owned();
            let outcome = outcome.to_owned();
            log.write(move |store| store.with(|db| {
                db.execute("UPDATE model_requests SET outcome=?2,http_status=?3,duration_ms=?4 WHERE request_id=?1",
                    rusqlite::params![id, outcome, status, elapsed.as_millis().min(i64::MAX as u128) as i64])
                    .map_err(|e| crate::error("request_log", e))?;
                Ok(())
            }))?;
        }
        Ok(())
    }
}
impl ModelBudget {
    /// Dropping this future while waiting cancels admission, without charging a call.
    pub(super) async fn admit(&self) -> Result<(), YourAiError> {
        loop {
            let wait_until = {
                let mut deadlines = self.control.deadlines.lock().unwrap();
                let now = Instant::now();
                let until = deadlines.0.max(deadlines.1);
                if until <= now {
                    self.reserve();
                    deadlines.0 = now
                        + self
                            .control
                            .policy
                            .rpm
                            .map(|rpm| Duration::from_secs_f64(60.0 / rpm as f64))
                            .unwrap_or_default();
                    return Ok(());
                }
                until
            };
            tokio::time::sleep_until(wait_until).await;
            // Another caller may have extended cooldown or taken the next paced slot.
        }
    }
}
pub(super) fn transient_limit(error: &YourAiError) -> bool {
    let Some((429, body)) = error.model_http_error() else {
        return false;
    };
    let body: serde_json::Value = serde_json::from_str(body).unwrap_or_default();
    !matches!(
        body.pointer("/error/code").and_then(|v| v.as_str()),
        Some("insufficient_quota" | "billing_hard_limit_reached")
    )
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
            (value * scale).ceil().min(i32::MAX as f64) as u64,
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
        .map(|d| d.min(Duration::from_millis(i32::MAX as u64)))
}

#[cfg(test)]
mod tests {
    use super::*;
    fn budget(policy: RequestPolicy) -> std::sync::Arc<ModelBudget> {
        ModelBudget::configured(
            policy,
            SqliteStore::open(std::path::Path::new(":memory:")).unwrap(),
        )
        .unwrap()
    }
    fn http_error(header: &str, value: &str) -> YourAiError {
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(
            header
                .to_owned()
                .parse::<reqwest::header::HeaderName>()
                .unwrap(),
            value.parse().unwrap(),
        );
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
    #[tokio::test(start_paused = true)]
    async fn concurrent_callers_get_distinct_paced_slots() {
        let budget = budget(RequestPolicy {
            rpm: Some(2),
            ..Default::default()
        });
        let start = Instant::now();
        let (a, b, c) = tokio::join!(
            async {
                budget.admit().await.unwrap();
                start.elapsed()
            },
            async {
                budget.admit().await.unwrap();
                start.elapsed()
            },
            async {
                budget.admit().await.unwrap();
                start.elapsed()
            },
        );
        let mut slots = [a.as_secs(), b.as_secs(), c.as_secs()];
        slots.sort();
        assert_eq!(slots, [0, 30, 60]);
        assert_eq!(budget.snapshot().calls, 3);
    }
    #[tokio::test(start_paused = true)]
    async fn pace_and_cancel_wait_without_counting_attempts() {
        let budget = budget(RequestPolicy {
            rpm: Some(2),
            ..Default::default()
        });
        budget.admit().await.unwrap();
        assert_eq!(budget.snapshot().calls, 1);
        assert!(
            tokio::time::timeout(Duration::from_secs(10), budget.admit())
                .await
                .is_err()
        );
        assert_eq!(budget.snapshot().calls, 1);
        let start = Instant::now();
        budget.admit().await.unwrap();
        assert_eq!(start.elapsed(), Duration::from_secs(20));
        assert_eq!(budget.snapshot().calls, 2);
    }
    #[tokio::test(start_paused = true)]
    async fn cooldown_is_shared_and_can_be_extended_during_wait() {
        let budget = budget(RequestPolicy {
            cooldown_seconds: 60,
            ..Default::default()
        });
        budget.control.cool_down(None);
        let other = budget.clone();
        let waiting = tokio::spawn(async move { other.admit().await.unwrap() });
        tokio::time::sleep(Duration::from_secs(30)).await;
        budget.control.cool_down(None);
        tokio::time::sleep(Duration::from_secs(30)).await;
        assert_eq!(budget.snapshot().calls, 0);
        waiting.await.unwrap();
        assert_eq!(budget.snapshot().calls, 1);
    }
}

#[cfg(test)]
mod provider_tests {
    use super::*;
    #[tokio::test(start_paused = true)]
    async fn provider_gates_are_independent_and_accounting_is_shared() {
        let store = SqliteStore::open(std::path::Path::new(":memory:")).unwrap();
        let a = ModelBudget::configured(
            RequestPolicy {
                rpm: Some(1),
                cooldown_seconds: 60,
            },
            store.clone(),
        )
        .unwrap();
        let b = a.for_provider(RequestPolicy::default()).unwrap();
        a.admit().await.unwrap();
        a.control.cool_down(None);
        tokio::time::timeout(Duration::from_millis(1), b.admit())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(a.snapshot().calls, 2);
        assert_eq!(b.snapshot().calls, 2);
        assert!(!a.control.remaining().is_zero());
        assert!(b.control.remaining().is_zero());
    }
}
