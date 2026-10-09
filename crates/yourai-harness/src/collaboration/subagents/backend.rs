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
        parent: &'a SessionHost,
        id: SessionId,
        tc: &'a ToolContext<'_>,
    ) -> BoxFuture<'a, Result<Arc<SessionHost>, YourAiError>> {
        Box::pin(async move {
            let root = parent
                .context()
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
                None => parent.agent().ctx().snapshot()?,
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
                .create_session(id.clone(), &history.system_prompt())
                .await?;
            meta.parent_session_id = Some(parent.context().id);
            catalog.save_session(&meta).await?;
            let child = crate::runtime::restore(
                &root,
                id,
                tc.cwd
                    .map(Path::to_owned)
                    .unwrap_or_else(|| parent.context().cwd),
                model,
                snapshot.hooks.clone(),
                self.tools.clone(),
                tc.security.clone(),
                tc.sandbox.clone(),
                history.policy(),
                "startup",
            )
            .await?;
            Ok(child)
        })
    }
}
