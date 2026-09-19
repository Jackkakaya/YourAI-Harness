//! Concrete implementations and assembly of the YourAI core contracts.
pub mod assembly;
pub mod collaboration;
pub mod context;
pub mod hooks;
pub mod memory;
pub mod model;
pub mod runtime;
pub mod security;
pub mod skills;
pub mod storage;
pub mod tools;
pub mod workspace;

pub use assembly::{Harness, HarnessConfig};
pub use context::{prompt::PromptConfig, MemoryContext};
pub use model::{GenaiModel, MeteredModel, ModelBudget};
pub use runtime::{HostConfig, SessionHost};
pub use security::PolicySecurity;
pub use storage::{SessionCatalog, SqliteStore};
pub use tools::ToolSet;

pub(crate) fn error(name: &'static str, e: impl std::fmt::Display) -> yourai_core::YourAiError {
    yourai_core::ErrorKind::Provider {
        name,
        message: e.to_string(),
    }
    .into()
}
