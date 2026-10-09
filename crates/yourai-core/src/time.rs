//! Optional policy bounds: absence of a timer never disables caller cancellation.
use std::{
    future::Future,
    time::{Duration, Instant},
};

pub(crate) async fn sleep_until(deadline: Option<Instant>) {
    match deadline {
        Some(deadline) => tokio::time::sleep_until(deadline.into()).await,
        None => std::future::pending().await,
    }
}

pub(crate) async fn timeout<T>(
    duration: Option<Duration>,
    future: impl Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    match duration {
        Some(duration) => tokio::time::timeout(duration, future).await,
        None => Ok(future.await),
    }
}
