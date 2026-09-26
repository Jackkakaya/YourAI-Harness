//! One time sample per prepared frame, captured at the UI edge. Render helpers
//! never query clocks themselves, so animations and expiry can be replayed.
use std::time::{Instant, SystemTime, UNIX_EPOCH};
#[derive(Clone, Copy)]
pub(super) struct FrameTime {
    pub monotonic: Instant,
    pub unix_seconds: i64,
}
impl FrameTime {
    pub fn now() -> Self {
        Self {
            monotonic: Instant::now(),
            unix_seconds: SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64)
                .unwrap_or(0),
        }
    }
}
