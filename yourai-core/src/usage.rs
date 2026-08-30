//! UsageTracker：token 用量与成本统计（用协议自有 Usage 类型）。

use crate::error::YourAiError;
use crate::future::BoxFuture;
use crate::session::SessionId;
use yourai_protocol::Usage;

#[derive(Debug, Clone, Default)]
pub struct UsageStats {
    pub total_input_tokens: u64,
    pub total_output_tokens: u64,
    pub total_tokens: u64,
    pub request_count: u64,
}

pub trait UsageTracker: Send + Sync {
    fn record(&self, session_id: &SessionId, usage: &Usage) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn total(&self) -> BoxFuture<'_, Result<UsageStats, YourAiError>>;
    fn session_usage(&self, session_id: &SessionId) -> BoxFuture<'_, Result<UsageStats, YourAiError>>;
    fn reset_session(&self, session_id: &SessionId) -> BoxFuture<'_, Result<(), YourAiError>>;
}
