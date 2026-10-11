//! Small typed document access; database revisions reject lost updates.
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::BTreeMap, sync::Arc};
use yourai_core::prelude::*;
pub(crate) struct Table<T> {
    store: Arc<dyn SessionStore>,
    id: SessionId,
    key: &'static str,
    _entry: std::marker::PhantomData<T>,
}
impl<T: Clone + Serialize + DeserializeOwned> Table<T> {
    pub(crate) fn new(store: Arc<dyn SessionStore>, id: SessionId, key: &'static str) -> Self {
        Self {
            store,
            id,
            key,
            _entry: std::marker::PhantomData,
        }
    }
    pub(crate) fn read(&self) -> Result<BTreeMap<String, T>, YourAiError> {
        self.store
            .read_state(&self.id, self.key)?
            .map(|s| serde_json::from_value(s.value).map_err(|e| crate::error("storage", e)))
            .transpose()
            .map(|v| v.unwrap_or_default())
    }
    pub(crate) fn update(
        &self,
        f: impl FnOnce(&mut BTreeMap<String, T>),
    ) -> Result<(), YourAiError> {
        let saved = self.store.read_state(&self.id, self.key)?;
        let revision = saved.as_ref().map(|s| s.revision);
        let mut values = saved
            .map(|s| serde_json::from_value(s.value).map_err(|e| crate::error("storage", e)))
            .transpose()?
            .unwrap_or_default();
        f(&mut values);
        self.store.save_state(
            &self.id,
            self.key,
            revision,
            serde_json::to_value(values).map_err(|e| crate::error("storage", e))?,
        )?;
        Ok(())
    }
}
