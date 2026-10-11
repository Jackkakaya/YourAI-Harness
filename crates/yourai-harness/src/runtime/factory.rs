//! Session factories assemble providers, then enter the core lifecycle template.
use super::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use yourai_core::prelude::*;
pub async fn create(
    root: &Path,
    cwd: PathBuf,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookManager>>,
    tools: Option<Arc<dyn ToolRegistry>>,
) -> Result<Arc<SessionManager>, YourAiError> {
    let config = crate::HarnessConfig::new(root.to_owned(), cwd);
    open(config, model, hooks, tools, None, None, "startup").await
}
#[allow(clippy::too_many_arguments)]
pub async fn restore(
    root: &Path,
    id: SessionId,
    cwd: PathBuf,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookManager>>,
    tools: Option<Arc<dyn ToolRegistry>>,
    security: Option<Arc<dyn SecurityProvider>>,
    sandbox: Option<Arc<dyn SandboxProvider>>,
    policy: ContextPolicy,
    source: &str,
) -> Result<Arc<SessionManager>, YourAiError> {
    let mut config = crate::HarnessConfig::new(root.to_owned(), cwd);
    config.resume = Some(id);
    config.context_policy = policy;
    open(config, model, hooks, tools, security, sandbox, source).await
}
#[allow(clippy::too_many_arguments)]
async fn open(
    config: crate::HarnessConfig,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookManager>>,
    tools: Option<Arc<dyn ToolRegistry>>,
    security: Option<Arc<dyn SecurityProvider>>,
    sandbox: Option<Arc<dyn SandboxProvider>>,
    source: &str,
) -> Result<Arc<SessionManager>, YourAiError> {
    config.context_policy.validate_for(model.token_budget())?;
    let catalog = Arc::new(crate::SessionCatalog::new(&config.root)?);
    let (meta, lease, prepared) =
        crate::assembly::prepare_session(&catalog, &config, model.as_ref()).await?;
    let id = meta.id;
    let usage = Arc::new(crate::storage::LocalUsage((*catalog.store).clone()));
    let (agent, _) = crate::assembly::assemble(
        &catalog,
        &id,
        &config.cwd,
        false,
        model,
        hooks,
        Some(usage),
        tools,
        config.context_policy,
    )
    .await?;
    if let Some(security) = security {
        agent.ctx().set_security(security);
    }
    if let Some(sandbox) = sandbox {
        agent.ctx().set_sandbox(sandbox);
    }
    let mut context = SessionContext::new(id, config.cwd);
    context.instructions = prepared.instructions;
    context.transcript_path = Some(crate::SqliteStore::path(&config.root));
    SessionManager::open_owned(lease, context, agent, SessionConfig::default(), source).await
}
