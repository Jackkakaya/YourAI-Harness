use crate::error;
pub mod sqlite;
pub(crate) mod table;
mod usage;
pub use sqlite::SqliteStore;
pub use usage::LocalUsage;

use fs2::FileExt;
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::{Path, PathBuf},
    sync::Arc,
};
use yourai_core::prelude::*;

pub(crate) fn atomic_write(path: &Path, value: &impl Serialize) -> Result<(), YourAiError> {
    let parent = path
        .parent()
        .ok_or_else(|| error("storage", "missing parent"))?;
    fs::create_dir_all(parent).map_err(|e| error("storage", e))?;
    let tmp = parent.join(format!(".{}.tmp", uuid::Uuid::new_v4()));
    let result = (|| {
        let mut f = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&tmp)
            .map_err(|e| error("storage", e))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            f.set_permissions(fs::Permissions::from_mode(0o600))
                .map_err(|e| error("storage", e))?;
        }
        f.write_all(&serde_json::to_vec(value).map_err(|e| error("storage", e))?)
            .map_err(|e| error("storage", e))?;
        f.sync_all().map_err(|e| error("storage", e))?;
        fs::rename(&tmp, path).map_err(|e| error("storage", e))?;
        File::open(parent)
            .and_then(|f| f.sync_all())
            .map_err(|e| error("storage", e))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(tmp);
    }
    result
}
pub(crate) fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, YourAiError> {
    serde_json::from_slice(&fs::read(path).map_err(|e| error("storage", e))?)
        .map_err(|e| error("storage", e))
}
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
        Ok(Self { root, store })
    }
    pub fn directory(&self, id: &SessionId) -> Result<PathBuf, YourAiError> {
        if id.0.is_empty() || !id.0.chars().all(|c| c.is_ascii_alphanumeric() || c == '-') {
            return Err(error("storage", "invalid session id"));
        }
        let path = self.root.join(&id.0);
        fs::create_dir_all(&path).map_err(|e| error("storage", e))?;
        Ok(path)
    }
}
impl SessionManager for SessionCatalog {
    fn initialize_system<'a>(
        &'a self,
        id: &'a SessionId,
        system: &'a str,
    ) -> BoxFuture<'a, Result<String, YourAiError>> {
        self.store.initialize_system(id, system)
    }

    fn create_session<'a>(
        &'a self,
        system: &'a str,
    ) -> BoxFuture<'a, Result<SessionMeta, YourAiError>> {
        self.store.create_session(system)
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
