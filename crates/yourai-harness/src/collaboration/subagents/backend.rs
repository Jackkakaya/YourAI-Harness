//! Private child session business actions.
use super::*;

pub(super) struct ChildPlan {
    pub(super) parent: Arc<SessionHost>,
    pub(super) root: PathBuf,
    pub(super) id: SessionId,
}
impl SubagentTool {
    pub(super) async fn run_prepare(&self) -> Result<ChildPlan, YourAiError> {
        let parent = self
            .host
            .upgrade()
            .ok_or_else(|| error("subagent", "parent session gone"))?;
        let root = parent
            .context()
            .transcript_path
            .as_ref()
            .and_then(|p| p.parent())
            .map(Path::to_path_buf)
            .unwrap_or_else(|| parent.dir.join("children"));
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
            .create_session(&parent.agent.ctx().context_manager()?.system_prompt())
            .await?;
        meta.parent_session_id = Some(parent.context().id);
        catalog.save_session(&meta).await?;
        Ok(ChildPlan {
            parent,
            root,
            id: meta.id,
        })
    }
    pub(super) async fn run_open_child(
        &self,
        plan: &ChildPlan,
        tc: &ToolContext<'_>,
    ) -> Result<Arc<SessionHost>, YourAiError> {
        let id = plan.id.as_str().to_owned();
        let child = SessionHost::restore(
            &plan.root,
            plan.id.clone(),
            plan.parent.context().cwd,
            self.model.clone(),
            plan.parent.agent.ctx().try_hooks(),
            self.tools.clone(),
            plan.parent.agent.ctx().context_manager()?.policy(),
            "startup",
        )
        .await?;
        if let Some(security) = tc.security.as_ref().filter(|s| s.bypass_approvals()) {
            child.agent.ctx().set_security(security.clone());
        }
        plan.parent.register_child(&child);
        self.children
            .lock()
            .unwrap()
            .insert(id.clone(), child.clone());
        Ok(child)
    }
    pub(super) async fn run_child_turn(
        &self,
        child: &Arc<SessionHost>,
        tc: &ToolContext<'_>,
        id: &str,
    ) -> Result<Option<String>, YourAiError> {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let work = child.run_next(TurnLimits::default(), &tx, tc.cancel);
        tokio::pin!(work);
        let report = loop {
            tokio::select! {biased;
                result=&mut work=>break result?,
                Some(event)=rx.recv()=>match event{
                    Out::Ask{id:request_id,payload}=>{let reply=tc.ask(InteractionKind::Question,json!({"child_id":id,"request":payload})).await?;child.submit_async(In::Reply{id:request_id,payload:reply}).await.map_err(|e|error("subagent",e))?;},
                    other=>{if !tc.emit_progress(json!({"child_id":id,"event":other})){child.interrupt();return Err(AbortReason::Disconnected.into());}}
                }
            }
        };
        if let Some(report) = report {
            return Ok(Some(report.result.map_err(|e| *e.error)?.text));
        }
        Ok(None)
    }
}
pub(super) struct ChildCleanup {
    pub(super) children: Arc<Mutex<HashMap<String, Arc<SessionHost>>>>,
    pub(super) id: String,
    pub(super) child: Arc<SessionHost>,
}
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        // Release the entry on every exit path: completion, error, or future cancellation.
        self.children.lock().unwrap().remove(&self.id);
        if self.child.status() == SessionStatus::Closed {
            return;
        }
        self.child.interrupt();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let child = self.child.clone();
            runtime.spawn(async move {
                let _ = child.close(None).await;
            });
        }
    }
}
