use crate::{error, prelude::*};
use std::{fs, path::Path};
pub fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T, YourAiError> {
    serde_json::from_slice(&fs::read(path).map_err(|e| error("storage", e))?)
        .map_err(|e| error("storage", e))
}
