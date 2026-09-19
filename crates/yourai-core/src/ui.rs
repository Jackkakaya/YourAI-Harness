//! OutSink：loop → 外界的同步事件出口（机制，词汇在 core::protocol）。

use crate::protocol::Out;

/// loop → 外界的同步出口。
///
/// 实现内部推 channel（TUI/Web），**永不阻塞 loop**；
/// 返回值告知接收端是否仍然存活：
/// - `true`：已投递（或实现方选择了丢弃策略）
/// - `false`：消费端已关闭——loop 应尽快以 `Aborted(Disconnected)` 中止
pub trait OutSink: Send + Sync {
    fn send(&self, m: Out) -> bool;

    /// 等待消费端关闭。无关闭信号的同步 sink 保持 pending；发送失败仍然有效。
    fn closed(&self) -> crate::BoxFuture<'_, ()> {
        Box::pin(std::future::pending())
    }
}
