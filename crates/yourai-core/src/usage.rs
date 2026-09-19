//! UsageTracker：token 用量与成本统计（用协议自有 Usage 类型）。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::session::SessionId;

#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
}

/// One provider response. Reuse id when retrying a failed accounting write.
#[derive(Debug, Clone)]
pub struct UsageEvent {
    pub id: String,
    pub model: Option<String>,
    pub source: String,
    pub usage: crate::chat::GenaiUsage,
}
impl UsageEvent {
    pub fn new(
        model: Option<String>,
        source: impl Into<String>,
        usage: crate::chat::GenaiUsage,
    ) -> Self {
        Self {
            id: uuid::Uuid::new_v4().to_string(),
            model,
            source: source.into(),
            usage,
        }
    }
}
pub trait UsageTracker: Send + Sync {
    fn record_event<'a>(
        &'a self,
        session_id: &'a SessionId,
        event: &'a UsageEvent,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;

    fn total<'a>(&'a self) -> BoxFuture<'a, Result<UsageStats, YourAiError>>;
    fn session_usage<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, Result<UsageStats, YourAiError>>;
    fn reset_session<'a>(
        &'a self,
        session_id: &'a SessionId,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
}
