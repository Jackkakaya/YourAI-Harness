//! MemoryManager：跨会话的长期记忆（用户偏好、项目上下文、事实等）。

use crate::error::YourAiError;
use crate::future::BoxFuture;

#[derive(Debug, Clone)]
pub struct MemoryEntry {
    pub key: String,
    pub value: String,
    pub category: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

pub trait MemoryManager: Send + Sync {
    fn store(&self, key: &str, value: &str) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn retrieve(&self, key: &str) -> BoxFuture<'_, Result<Option<String>, YourAiError>>;
    fn search(&self, query: &str, limit: usize)
        -> BoxFuture<'_, Result<Vec<MemoryEntry>, YourAiError>>;
    fn list(&self, category: Option<&str>) -> BoxFuture<'_, Result<Vec<MemoryEntry>, YourAiError>>;
    fn delete(&self, key: &str) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn clear(&self) -> BoxFuture<'_, Result<(), YourAiError>>;
}
