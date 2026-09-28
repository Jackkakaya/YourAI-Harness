//! Ordered request journal. Drop paths enqueue; a flush acknowledges durable writes.
use super::SqliteStore;
use std::sync::{
    atomic::{AtomicU64, Ordering},
    mpsc, Arc,
};
use yourai_core::prelude::*;

type Write = Box<dyn FnOnce(&SqliteStore) -> Result<(), YourAiError> + Send>;
enum Job {
    Write(Write),
    Flush(tokio::sync::oneshot::Sender<Result<(), String>>),
}
pub(crate) struct RequestLog {
    tx: mpsc::Sender<Job>,
    errors: Arc<AtomicU64>,
}
impl RequestLog {
    pub fn new(store: SqliteStore) -> Result<Arc<Self>, YourAiError> {
        let (tx, rx) = mpsc::channel();
        let errors = Arc::new(AtomicU64::new(0));
        let worker_errors = errors.clone();
        std::thread::Builder::new()
            .name("yourai-request-log".into())
            .spawn(move || {
                let mut last_error = None;
                for job in rx {
                    match job {
                        Job::Write(write) => {
                            if let Err(error) = write(&store) {
                                worker_errors.fetch_add(1, Ordering::Relaxed);
                                last_error = Some(error.to_string());
                            }
                        }
                        Job::Flush(reply) => {
                            let _ = reply.send(last_error.clone().map_or(Ok(()), Err));
                        }
                    }
                }
            })
            .map_err(|e| crate::error("request_log", e))?;
        Ok(Arc::new(Self { tx, errors }))
    }
    pub fn errors(&self) -> u64 {
        self.errors.load(Ordering::Relaxed)
    }
    pub fn write(
        &self,
        write: impl FnOnce(&SqliteStore) -> Result<(), YourAiError> + Send + 'static,
    ) -> Result<(), YourAiError> {
        self.tx
            .send(Job::Write(Box::new(write)))
            .map_err(|_| crate::error("request_log", "writer stopped"))
    }
    pub async fn flush(&self) -> Result<(), YourAiError> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.tx
            .send(Job::Flush(tx))
            .map_err(|_| crate::error("request_log", "writer stopped"))?;
        rx.await
            .map_err(|e| crate::error("request_log", e))?
            .map_err(|e| crate::error("request_log", e))
    }
}
