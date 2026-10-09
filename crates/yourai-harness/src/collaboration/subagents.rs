mod backend;
use crate::SessionHost;
use std::{
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
};
use yourai_core::prelude::*;
pub use yourai_core::subagent::Subagent;

/// Assemble the one core subagent tool without another execution object.
pub fn subagent(
    host: &Arc<SessionHost>,
    model: Option<Arc<dyn ModelProvider>>,
    tools: Option<Arc<dyn ToolRegistry>>,
) -> Arc<Subagent> {
    Arc::new(Subagent::new(
        host,
        Arc::new(backend::ChildFactory {
            model,
            tools,
            catalog: Mutex::new(None),
        }),
        "worker".into(),
        3,
    ))
}
