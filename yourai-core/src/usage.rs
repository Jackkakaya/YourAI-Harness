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
    fn record<'a>(
        &'a self,
        session_id: &'a SessionId,
        usage: &'a Usage,
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
