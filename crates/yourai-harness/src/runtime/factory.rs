//! Session factories assemble providers, then enter the core lifecycle template.
use super::*;
use std::{
    path::{Path, PathBuf},
    sync::Arc,
};
use tokio_util::sync::CancellationToken;
use yourai_core::prelude::*;
use yourai_core::runtime::SessionLease;
pub async fn create(
    root: &Path,
    cwd: PathBuf,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
    tools: Option<Arc<dyn ToolRegistry>>,
) -> Result<Arc<SessionHost>, YourAiError> {
    let catalog = Arc::new(crate::SessionCatalog::new(root)?);
    let prompt = crate::context::prompt::prepare(
        &crate::PromptConfig::default(),
        &cwd,
        None,
        &[],
        None,
        None,
        &ContextPolicy::default(),
        &CancellationToken::new(),
    )
    .await?;
    let meta = catalog
        .create_session(SessionId::new(), &prompt.system)
        .await?;
    restore(
        root,
        meta.id,
        cwd,
        model,
        hooks,
        tools,
        None,
        None,
        ContextPolicy::default(),
        "startup",
    )
    .await
}
#[allow(clippy::too_many_arguments)]
pub async fn restore(
    root: &Path,
    id: SessionId,
    cwd: PathBuf,
    model: Arc<dyn ModelProvider>,
    hooks: Option<Arc<dyn HookRuntime>>,
    tools: Option<Arc<dyn ToolRegistry>>,
    security: Option<Arc<dyn SecurityProvider>>,
    sandbox: Option<Arc<dyn SandboxProvider>>,
    policy: ContextPolicy,
    source: &str,
) -> Result<Arc<SessionHost>, YourAiError> {
    let catalog = Arc::new(crate::SessionCatalog::new(root)?);
    let lease = SessionLease::acquire(catalog.directory(&id)?)?;
    let mut meta = catalog.load_session(&id).await?;
    if meta.system_prompt.is_none() {
        let prompt = crate::context::prompt::prepare(
            &crate::PromptConfig::default(),
            &cwd,
            None,
            &[],
            None,
            None,
            &policy,
            &CancellationToken::new(),
        )
        .await?;
        meta.system_prompt = Some(catalog.initialize_system(&id, &prompt.system).await?);
    }
    meta.model = Some(model.model_iden().into());
    catalog.save_session(&meta).await?;
    let usage = Arc::new(crate::storage::LocalUsage((*catalog.store).clone()));
    let (agent, _) = crate::assembly::assemble(
        &catalog,
        &id,
        &cwd,
        false,
        model,
        hooks,
        Some(usage),
        tools,
        policy,
    )
    .await?;
    if let Some(security) = security {
        agent.ctx().set_security(security);
    }
    if let Some(sandbox) = sandbox {
        agent.ctx().set_sandbox(sandbox);
    }
    let mut context = SessionContext::new(id, cwd);
    context.transcript_path = Some(crate::SqliteStore::path(root));
    SessionHost::open_owned(lease, context, agent, HostConfig::default(), source).await
}
