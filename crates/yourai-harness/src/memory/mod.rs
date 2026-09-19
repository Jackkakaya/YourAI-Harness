mod hooks;
use crate::storage::table::Table;
pub(crate) use hooks::register;
use std::{
    path::PathBuf,
    time::{SystemTime, UNIX_EPOCH},
};
use yourai_core::prelude::*;
pub struct LocalMemory(Table<MemoryEntry>);
impl LocalMemory {
    pub fn open(path: PathBuf) -> Result<Self, YourAiError> {
        Ok(Self(Table::open(path)?))
    }
}
impl MemoryManager for LocalMemory {
    fn store<'a>(
        &'a self,
        key: &'a str,
        value: &'a str,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
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
        })
    }
    fn retrieve<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<Option<String>, YourAiError>> {
        Box::pin(async move {
            Ok(self
                .0
                .data
                .lock()
                .unwrap()
                .get(key)
                .map(|e| e.value.clone()))
        })
    }
    fn search<'a>(
        &'a self,
        query: &'a str,
        limit: usize,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>> {
        Box::pin(async move {
            let words: Vec<_> = query.split_whitespace().map(str::to_lowercase).collect();
            let mut entries: Vec<_> = self
                .0
                .data
                .lock()
                .unwrap()
                .values()
                .filter_map(|e| {
                    let text = format!("{} {}", e.key, e.value).to_lowercase();
                    let score = words.iter().filter(|w| text.contains(w.as_str())).count();
                    (score > 0).then(|| (score, e.clone()))
                })
                .collect();
            entries.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.key.cmp(&b.1.key)));
            Ok(entries.into_iter().take(limit).map(|(_, e)| e).collect())
        })
    }
    fn list<'a>(
        &'a self,
        category: Option<&'a str>,
    ) -> BoxFuture<'a, Result<Vec<MemoryEntry>, YourAiError>> {
        Box::pin(async move {
            Ok(self
                .0
                .data
                .lock()
                .unwrap()
                .values()
                .filter(|e| category.is_none() || e.category.as_deref() == category)
                .cloned()
                .collect())
        })
    }
    fn delete<'a>(&'a self, key: &'a str) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            self.0.update(|d| {
                d.remove(key);
            })
        })
    }
    fn clear<'a>(&'a self) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async { self.0.update(|d| d.clear()) })
    }
}
impl MemoryProvider for LocalMemory {
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
