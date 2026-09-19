use crate::storage::{atomic_write, read_json};
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::BTreeMap, path::PathBuf, sync::Mutex};
use yourai_core::YourAiError;
pub(crate) struct Table<T> {
    path: PathBuf,
    pub(crate) data: Mutex<BTreeMap<String, T>>,
}
impl<T: Clone + Serialize + DeserializeOwned> Table<T> {
    pub(crate) fn open(path: PathBuf) -> Result<Self, YourAiError> {
        let data = if path.exists() {
            read_json(&path)?
        } else {
            BTreeMap::new()
        };
        Ok(Self {
            path,
            data: Mutex::new(data),
        })
    }
    pub(crate) fn update(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, T>),
    ) -> Result<(), YourAiError> {
        let mut guard = self.data.lock().unwrap();
        let mut next = guard.clone();
        f(&mut next);
        atomic_write(&self.path, &next)?;
        *guard = next;
        Ok(())
    }
}
