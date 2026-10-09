//! Child assembly has no hook protocol or execution lifecycle.
use super::*;
use yourai_core::subagent::SubagentFactory;
pub(super) struct ChildFactory {
    pub model: Arc<dyn ModelProvider>,
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
            let mut meta = catalog
                .create_session(
                    id.clone(),
                    &parent.agent().ctx().context_manager()?.system_prompt(),
                )
                .await?;
            meta.parent_session_id = Some(parent.context().id);
            catalog.save_session(&meta).await?;
            let child = crate::runtime::restore(
                &root,
                id,
                tc.cwd
                    .map(Path::to_owned)
                    .unwrap_or_else(|| parent.context().cwd),
                self.model.clone(),
                parent.agent().ctx().try_hooks(),
                self.tools.clone(),
                tc.security.clone(),
                tc.sandbox.clone(),
                parent.agent().ctx().context_manager()?.policy(),
                "startup",
            )
            .await?;
            Ok(child)
        })
    }
}
