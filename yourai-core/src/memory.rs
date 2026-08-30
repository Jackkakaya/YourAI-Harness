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
    /// 存一条记忆；`category` 用于 [`list`](MemoryManager::list) 的分类筛选
    fn store<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn retrieve<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<String>, YourAiError>>;
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>>;
    fn list<'a>(
        &'a self,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>>;
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), YourAiError>>;
    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), YourAiError>>;
}
