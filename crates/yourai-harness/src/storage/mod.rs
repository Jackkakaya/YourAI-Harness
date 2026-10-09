use crate::error;
pub(crate) mod request_log;
pub mod sqlite;
pub(crate) mod table;
mod usage;
pub use sqlite::SqliteStore;
pub use usage::LocalUsage;

use fs2::FileExt;
use std::{
    fs::{self, File, OpenOptions},
    path::{Path, PathBuf},
    sync::Arc,
};
use yourai_core::prelude::*;

pub(crate) use yourai_core::file_store::{atomic_write, read_json};
fn lock(path: &Path) -> Result<File, YourAiError> {
    let f = OpenOptions::new()
        .create(true)
        .truncate(false)
        .read(true)
        .write(true)
        .open(path)
        .map_err(|e| error("storage", e))?;
    f.try_lock_exclusive()
        .map_err(|e| error("storage", format!("session already open: {e}")))?;
    Ok(f)
}

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
impl SessionManager for SessionCatalog {
    fn read_tasks(&self, id: &SessionId) -> Result<Vec<Task>, YourAiError> {
        self.store.read_tasks(id)
    }
    fn save_tasks(&self, id: &SessionId, tasks: Vec<Task>) -> Result<Vec<Task>, YourAiError> {
        self.store.save_tasks(id, tasks)
    }

    fn initialize_system<'a>(
        &'a self,
        id: &'a SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        self.store.initialize_system(id, system)
    }

    fn create_session<'a>(
        &'a self,
        id: SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.store.create_session(id, system)
    }
    fn load_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.store.load_session(id)
    }
    fn save_session<'a>(&'a self, m: &'a SessionMeta) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.store.save_session(m)
    }
    fn list_sessions<'a>(&'a self) -> BoxFuture<'a, Result<Vec<SessionMeta>, YourAiError>> {
        self.store.list_sessions()
    }
    fn delete_session<'a>(&'a self, id: &'a SessionId) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let dir = self.directory(id)?;
            let _guard = lock(&dir.join("host.lock"))?;
            self.store.delete_session(id).await?;
            fs::remove_dir_all(dir).map_err(|e| error("storage", e))
        })
    }
    fn fork_session<'a>(
        &'a self,
        id: &'a SessionId,
    ) -> BoxFuture<'a, Result<SessionId, YourAiError>> {
        self.store.fork_session(id)
    }
    fn read_messages<'a>(
        &'a self,
        id: &'a SessionId,
        q: MessageQuery,
    ) -> BoxFuture<'a, Result<MessagePage, YourAiError>> {
        self.store.read_messages(id, q)
    }
    fn append_messages<'a>(
        &'a self,
        id: &'a SessionId,
        m: Vec<StoredMessage>,
    ) -> BoxFuture<'a, Result<Vec<StoredMessage>, YourAiError>> {
        self.store.append_messages(id, m)
    }
    fn save_context<'a>(
        &'a self,
        id: &'a SessionId,
        c: ContextChange,
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        self.store.save_context(id, c)
    }
}
