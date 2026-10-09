use crate::{error, prelude::*};
use serde::Serialize;
use std::{
    fs::{self, File, OpenOptions},
    io::Write,
    path::Path,
};
pub fn atomic_write(path: &Path, value: &impl Serialize) -> Result<(), YourAiError> {
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
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, YourAiError> {
    serde_json::from_slice(&fs::read(path).map_err(|e| error("storage", e))?)
        .map_err(|e| error("storage", e))
}
