//! OutSink：loop → 外界的同步事件出口（机制，词汇在 yourai-protocol）。

use yourai_protocol::Out;

/// loop → 外界的同步 fire-and-forget 出口。
///
/// 实现内部推 channel（TUI/Web），**永不阻塞 loop**；
/// 消费端已离开时应静默丢弃。
pub trait OutSink: Send + Sync {
    fn send(&self, m: Out);
}
