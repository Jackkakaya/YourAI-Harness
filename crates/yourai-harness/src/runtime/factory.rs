//! Session factories assemble providers, then enter the core lifecycle template.
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use yourai_core::prelude::*;

/// Assemble a new session from dependency bindings. Runtime configuration stays
/// with SessionManager; context, storage and built-in tools are session-owned.
pub async fn create(
    root: &Path,
    cwd: PathBuf,
    bindings: AgentBuilder,
) -> Result<Arc<SessionManager>, YourAiError> {
    let config = crate::SessionOptions::new(root.to_owned(), cwd);
    open(config, bindings, "startup").await
}

/// Restore with explicit dependency bindings and independent runtime settings.
pub async fn restore(
    root: &Path,
    id: SessionId,
    cwd: PathBuf,
    bindings: AgentBuilder,
    settings: ExecutionOptions,
    source: &str,
) -> Result<Arc<SessionManager>, YourAiError> {
    let mut config = crate::SessionOptions::new(root.to_owned(), cwd);
    config.resume = Some(id);
    config.config.set_execution(settings);
    open(config, bindings, source).await
}

async fn open(
    config: crate::SessionOptions,
    mut bindings: AgentBuilder,
    source: &str,
) -> Result<Arc<SessionManager>, YourAiError> {
    let values = config.config.values();
    let model = bindings
        .model
        .as_ref()
        .ok_or_else(|| ErrorKind::Config("provider not configured: model".into()))?;
    values
        .execution()
        .context_policy()
        .validate_for(model.token_budget())?;
    let catalog = Arc::new(crate::SessionCatalog::new(&config.root)?);
    let (meta, lease, prepared) =
        crate::assembly::prepare_session(&catalog, &config, model.as_ref(), &values).await?;
    let id = meta.id;
    // Explicit usage implementations are dependencies, just like hooks/security.
    bindings
        .usage
        .get_or_insert_with(|| Arc::new(crate::storage::LocalUsage((*catalog.store).clone())));
    let (agent, _) = crate::assembly::assemble(
        &id,
        lease.store(),
        &config.cwd,
        values.security().trusted_shell(),
        bindings,
        values.execution(),
    )
    .await?;
    crate::assembly::start_session(&config, id, lease, agent, prepared, source, &values).await
}
