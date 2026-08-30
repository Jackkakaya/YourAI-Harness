//! ObservabilityProvider：trace + metrics（基于 trait 抽象，可桥接 tracing / OTel）。

use std::sync::Arc;

/// 一个逻辑 span（实现方可桥接到 tracing span / OTel span）
pub trait Span: Send + Sync {
    fn record(&self, key: &str, value: &str);
    fn record_error(&self, error: &str);
}

pub trait ObservabilityProvider: Send + Sync {
    fn span(&self, name: &str) -> Arc<dyn Span>;
    fn child_span(&self, name: &str, parent: &dyn Span) -> Arc<dyn Span>;

    /// 打点（实现方可路由到 metrics 后端）
    fn metric(&self, name: &str, value: f64, tags: &[(&str, &str)]);

    fn increment(&self, name: &str, tags: &[(&str, &str)]) {
        self.metric(name, 1.0, tags);
    }

    fn timing(&self, name: &str, seconds: f64, tags: &[(&str, &str)]) {
        self.metric(name, seconds, tags);
    }
}
