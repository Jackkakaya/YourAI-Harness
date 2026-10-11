mod hooks;
use crate::storage::table::Table;
pub(crate) use hooks::register;
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
use yourai_core::prelude::*;
pub struct LocalMemory(Table<MemoryEntry>);
impl LocalMemory {
    pub fn new(store: Arc<dyn SessionStore>, id: SessionId) -> Self {
        Self(Table::new(store, id, "memory"))
    }

    pub async fn store(
        &self,
        key: &str,
        value: &str,
        category: Option<&str>,
    ) -> Result<(), YourAiError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs() as i64;
        self.0.update(|d| {
            let created = d.get(key).map(|e| e.created_at).unwrap_or(now);
            d.insert(
                key.into(),
                MemoryEntry {
                    key: key.into(),
                    value: value.into(),
                    category: category.map(str::to_owned),
                    created_at: created,
                    updated_at: now,
                },
            );
        })
    }
    pub async fn retrieve(&self, key: &str) -> Result<Option<String>, YourAiError> {
        Ok(self.0.read()?.get(key).map(|e| e.value.clone()))
    }
    pub async fn search(&self, query: &str, limit: usize) -> Result<Vec<MemoryEntry>, YourAiError> {
        let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
        let mut entries: Vec<_> = self
            .0
            .read()?
            .values()
            .filter_map(|e| {
                let text = format!("{} {}", e.key, e.value).to_lowercase();
                let score = words.iter().filter(|w| text.contains(w.as_str())).count();
                (score > 0).then(|| (score, e.clone()))
            })
            .collect();
        entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.key.cmp(&b.1.key)));
        Ok(entries.into_iter().take(limit).map(|(_, e)| e).collect())
    }
    pub async fn list(&self, category: Option<&str>) -> Result<Vec<MemoryEntry>, YourAiError> {
        Ok(self
            .0
            .read()?
            .values()
            .filter(|e| category.is_none() || e.category.as_deref() == category)
            .cloned()
            .collect())
    }
    pub async fn delete(&self, key: &str) -> Result<(), YourAiError> {
        self.0.update(|d| {
            d.remove(key);
        })
    }
    pub async fn clear(&self) -> Result<(), YourAiError> {
        self.0.update(|d| d.clear())
    }
}
impl MemoryManager for LocalMemory {
    fn recall<'a>(
        &'a self,
        request: RecallRequest<'a>,
        cancel: &'a tokio_util::sync::CancellationToken,
    ) -> BoxFuture<'a, Result<Vec<RecalledMemory>, YourAiError>> {
        Box::pin(async move {
            if cancel.is_cancelled() {
                return Err(AbortReason::Cancelled.into());
            }
            Ok(self
                .search(request.query, request.limit)
                .await?
                .into_iter()
                .map(|e| RecalledMemory {
                    provider: "local".into(),
                    id: e.key,
                    content: e.value,
                    updated_at: Some(e.updated_at),
                })
                .collect())
        })
    }
}
