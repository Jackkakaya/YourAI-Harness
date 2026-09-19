//! Concrete host and optional extensions for the five default-loop flow diagrams.
pub mod extensions;
pub mod harness;
pub mod host;
pub mod local;
pub use harness::{Harness, HarnessConfig};
pub mod model_hooks;
pub mod providers;
pub mod sqlite;
pub mod storage;
pub use host::{HostConfig, SessionHost};
pub use providers::{GenaiModel, MeteredModel, ModelBudget, PolicySecurity, ToolSet};
pub use sqlite::SqliteStore;
pub use storage::SessionCatalog;
pub mod memory_context;
pub mod tool_result;
pub use memory_context::MemoryContext;

pub(crate) fn error(name: &'static str, e: impl std::fmt::Display) -> yourai_core::YourAiError {
    yourai_core::ErrorKind::Provider {
        name,
        message: e.to_string(),
    }
    .into()
}
