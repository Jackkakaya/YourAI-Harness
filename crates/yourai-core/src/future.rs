//! 统一的 boxed future 别名——全部 trait 用它保持 dyn 兼容（不引 async_trait）。

/// 所有 trait 异步方法的返回类型。
pub type BoxFuture<'a, T> = std::pin::Pin<Box<dyn std::future::Future<Output = T> + Send + 'a>>;
