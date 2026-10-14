//! Child assembly has no hook protocol or execution lifecycle.
use super::*;
use yourai_core::subagent::SubagentFactory;
pub(super) struct ChildFactory {
    pub model: Option<Arc<dyn ModelProvider>>,
    pub tools: Option<Arc<dyn ToolRegistry>>,
    pub catalog: Mutex<Option<(PathBuf, Arc<crate::SessionCatalog>)>>,
}
impl SubagentFactory for ChildFactory {
    fn create<'a>(
        &'a self,
        parent: &'a SessionManager,
        id: SessionId,
        tc: &'a ToolContext<'_>,
    ) -> BoxFuture<'a, Result<Arc<SessionManager>, YourAiError>> {
        Box::pin(async move {
            let root = parent
                .environment()
                .transcript_path
                .as_ref()
                .and_then(|p| p.parent())
                .map(Path::to_path_buf)
                .unwrap_or_else(|| parent.directory().join("children"));
            let catalog = {
                let mut cached = self.catalog.lock().unwrap();
                match &*cached {
                    Some((cached_root, catalog)) if cached_root == &root => catalog.clone(),
                    _ => {
                        let catalog = Arc::new(crate::SessionCatalog::new(&root)?);
                        *cached = Some((root.clone(), catalog.clone()));
                        catalog
                    }
                }
            };
            let snapshot = match tc.providers {
                Some(providers) => providers.clone(),
                None => parent.agent().snapshot()?,
            };
            let history = snapshot
                .context_manager
                .as_ref()
                .ok_or_else(|| crate::error("subagent", "context not configured"))?;
            let model = self
                .model
                .as_ref()
                .or(snapshot.model.as_ref())
                .cloned()
                .ok_or_else(|| crate::error("subagent", "model not configured"))?;
            let mut meta = catalog
                .store
                .create_session(id.clone(), &history.system_prompt())
                .await?;
            meta.parent_session_id = Some(parent.environment().id);
            catalog.store.save_session(&meta).await?;

            let mut settings = parent.config().clone();
            settings.set_context_policy(history.policy());
            // Child input activation is independent of the parent's selected skills/memory.
            settings.set_skill_ids(vec![]);
            settings.set_memory_search_limit(0);
            settings.set_instruction_paths(vec![]);
            settings.set_workspace_enabled(false);
            let mut bindings = Agent::builder().model(model);
            bindings.hooks = snapshot.hooks;
            bindings.tools = self.tools.clone();
            bindings.security = tc.security.clone();
            bindings.sandbox = tc.sandbox.clone();
            bindings.compaction_model = snapshot.compaction_model;
            crate::runtime::restore(
                &root,
                id,
                tc.cwd
                    .map(Path::to_owned)
                    .unwrap_or_else(|| parent.environment().cwd),
                bindings,
                settings,
                "startup",
            )
            .await
        })
    }
}
