use crate::error;
pub(crate) mod request_log;
pub mod sqlite;
pub(crate) mod table;
mod usage;
pub use sqlite::SqliteStore;
pub use usage::LocalUsage;

use std::{fs, path::PathBuf, sync::Arc};
use yourai_core::prelude::*;

pub(crate) use yourai_core::file_store::{atomic_write, read_json};
/// Session directories hold host/extension state, while all session records use SQLite.
pub struct SessionCatalog {
    root: PathBuf,
    pub(crate) tool_output: Arc<crate::tools::ToolOutputStore>,
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
                    return Err(error("storage","legacy JSON sessions found; run yourai-tui --import-json-sessions before opening"));
                }
            }
        }
        let tool_output = crate::tools::ToolOutputStore::open(root.join("tool-output"))?;
        Ok(Self {
            root,
            store,
            tool_output,
        })
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
