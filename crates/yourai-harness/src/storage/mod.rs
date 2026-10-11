use crate::error;
pub(crate) mod request_log;
pub mod sqlite;
pub(crate) mod table;
mod usage;
pub use sqlite::SqliteStore;
pub use usage::LocalUsage;

use std::{fs, path::PathBuf, sync::Arc};
use yourai_core::prelude::*;

pub(crate) use yourai_core::file_store::read_json;
/// SQLite owns Agent state; directories are only workspace locations and legacy import sources.
pub struct SessionCatalog {
    root: PathBuf,
    pub store: Arc<crate::SqliteStore>,
}
impl SessionCatalog {
    pub fn new(root: impl Into<PathBuf>) -> Result<Self, YourAiError> {
        let root = root.into();
        fs::create_dir_all(&root).map_err(|e| error("storage", e))?;
        let store = Arc::new(crate::SqliteStore::open(&crate::SqliteStore::path(&root))?);
        // Existing JSON is an untouched backup only after explicit import.
        for entry in fs::read_dir(&root).map_err(|e| error("storage", e))? {
            let path = entry.map_err(|e| error("storage", e))?.path();
            if path.is_dir()
                && (path.join("meta.json").exists() || path.join("history.json").exists())
            {
                let id = path
                    .file_name()
                    .and_then(|p| p.to_str())
                    .unwrap_or_default();
                let imported = store.with(|c| {
                    c.query_row(
                        "SELECT EXISTS(SELECT 1 FROM sessions WHERE session_id=?1)",
                        [id],
                        |r| r.get::<_, bool>(0),
                    )
                    .map_err(|e| error("storage", e))
                })?;
                if !imported {
                    return Err(error("storage","legacy JSON sessions found; run yourai --import-json-sessions before opening"));
                }
            }
        }
        Ok(Self { root, store })
    }
    /// One-way import of previous runtime files; normal reads/writes use the database only.
    pub(crate) fn import_state(
        &self,
        id: &SessionId,
        store: &dyn SessionStore,
    ) -> Result<(), YourAiError> {
        let directory = self.root.join(id.as_str());
        for (file, key) in [
            ("host.json", "runtime"),
            ("memory.json", "memory"),
            ("skills.json", "skills"),
            ("permissions.json", "permissions"),
            ("config.json", "config"),
            ("worktrees.json", "worktrees"),
        ] {
            if store.read_state(id, key)?.is_some() {
                continue;
            }
            let path = directory.join(file);
            if path.is_file() {
                let value = read_json(&path)?;
                store.save_state(id, key, None, value)?;
            }
        }
        Ok(())
    }
    /// Import referenced immutable legacy outputs, leaving the history and backups unchanged.
    pub(crate) async fn import_outputs(
        &self,
        id: &SessionId,
        store: Arc<dyn SessionStore>,
    ) -> Result<(), YourAiError> {
        use tokio::io::AsyncReadExt;
        if store.read_state(id, "output_import")?.is_some() {
            return Ok(());
        }
        let directory = self.root.join("tool-output");
        if !directory.is_dir() {
            return Ok(());
        }
        let directory = directory.canonicalize().map_err(|e| error("storage", e))?;
        let mut after = 0;
        let output = crate::tools::ToolOutputStore::new(store.clone(), id.clone());
        loop {
            let page = store
                .read_messages(
                    id,
                    MessageQuery {
                        after,
                        active_only: false,
                        limit: 1000,
                    },
                )
                .await?;
            let mut paths = std::collections::HashSet::new();
            for record in &page.messages {
                for response in record.message.content.tool_responses() {
                    let Ok(value) = serde_json::from_str::<serde_json::Value>(&response.content)
                    else {
                        continue;
                    };
                    if let Some(list) = value.get("output_paths").and_then(|v| v.as_array()) {
                        paths.extend(list.iter().filter_map(|v| v.as_str()).map(str::to_owned));
                    }
                }
            }
            for path in paths {
                let Some(reference) = crate::tools::ToolOutputStore::legacy_reference(&path) else {
                    continue;
                };
                let path = PathBuf::from(path);
                if !path.is_file() {
                    continue;
                }
                let canonical = path.canonicalize().map_err(|e| error("storage", e))?;
                if canonical.parent() != Some(directory.as_path()) {
                    continue;
                }
                let mut file = tokio::fs::File::open(canonical)
                    .await
                    .map_err(|e| error("storage", e))?;
                let mut buffer = vec![0; 128 * 1024];
                let mut offset = 0;
                loop {
                    let n = file
                        .read(&mut buffer)
                        .await
                        .map_err(|e| error("storage", e))?;
                    if n == 0 {
                        break;
                    }
                    store.renew_session(crate::runtime::SessionLease::TTL_MS)?;
                    output.append(&reference, offset, &buffer[..n]).await?;
                    offset += n as u64;
                }
            }
            match page.next {
                Some(next) => after = next,
                None => break,
            }
        }
        store.save_state(id, "output_import", None, serde_json::json!(true))?;
        Ok(())
    }
    pub fn directory(&self, id: &SessionId) -> Result<PathBuf, YourAiError> {
        if id.as_str().is_empty()
            || !id
                .as_str()
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-')
        {
            return Err(error("storage", "invalid session id"));
        }
        let path = self.root.join(id.as_str());
        fs::create_dir_all(&path).map_err(|e| error("storage", e))?;
        Ok(path)
    }
}
