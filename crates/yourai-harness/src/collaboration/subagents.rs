use crate::{error, SessionHost};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::Path,
    sync::{Arc, Mutex, Weak},
    time::Duration,
};
use yourai_core::prelude::*;
/// Subagents are independent sessions. Parent owns cancellation and observes child output.
pub struct SubagentTool {
    host: Weak<SessionHost>,
    model: Arc<dyn ModelProvider>,
    tools: Option<Arc<dyn ToolRegistry>>,
    children: Mutex<HashMap<String, Arc<SessionHost>>>,
}
impl SubagentTool {
    pub fn new(
        host: &Arc<SessionHost>,
        model: Arc<dyn ModelProvider>,
        tools: Option<Arc<dyn ToolRegistry>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            host: Arc::downgrade(host),
            model,
            tools,
            children: Mutex::new(HashMap::new()),
        })
    }
    pub fn child_ids(&self) -> Vec<String> {
        self.children.lock().unwrap().keys().cloned().collect()
    }
    pub async fn stop_all(&self) -> Result<(), YourAiError> {
        let children: Vec<_> = self.children.lock().unwrap().values().cloned().collect();
        for child in children {
            child.close(Duration::from_secs(10)).await?;
        }
        Ok(())
    }
    async fn run_child(&self, tc: ToolContext<'_>, prompt: String) -> Result<Value, YourAiError> {
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
        let catalog = crate::SessionCatalog::new(&root)?;
        let mut meta = catalog
            .create_session(&parent.agent.ctx().context_manager()?.system_prompt())
            .await?;
        meta.parent_session_id = Some(parent.context().id);
        catalog.save_session(&meta).await?;
        let id = meta.id.0.clone();
        let start = parent
            .dispatch(HookEvent::SubagentStart {
                agent_id: id.clone(),
                agent_type: "worker".into(),
            })
            .await?;
        parent.consume_hook(&start, true)?;
        let child = SessionHost::restore(
            &root,
            meta.id,
            parent.context().cwd,
            self.model.clone(),
            parent.agent.ctx().try_hooks(),
            self.tools.clone(),
            parent.agent.ctx().context_manager()?.policy(),
            "startup",
        )
        .await?;
        if let Some(security) = tc.security.as_ref().filter(|s| s.bypass_approvals()) {
            child.agent.ctx().set_security(security.clone());
        }
        parent.register_child(&child);
        self.children
            .lock()
            .unwrap()
            .insert(id.clone(), child.clone());
        let _cleanup = ChildCleanup(child.clone());
        child
            .submit(In::user_text(prompt))
            .map_err(|e| error("subagent", e))?;
        let mut last = String::new();
        for continuation in 0..=3 {
            let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
            let work = child.run_next(TurnLimits::default(), &tx, tc.cancel);
            tokio::pin!(work);
            let report = loop {
                tokio::select! {biased;
                    result=&mut work=>break result?,
                    Some(event)=rx.recv()=>match event{
                        Out::Ask{id:request_id,payload}=>{let reply=tc.ask(InteractionKind::Question,json!({"child_id":id,"request":payload})).await?;child.submit(In::Reply{id:request_id,payload:reply}).map_err(|e|error("subagent",e))?;},
                        other=>{if !tc.emit_progress(json!({"child_id":id,"event":other})){child.interrupt();return Err(AbortReason::Disconnected.into());}}
                    }
                }
            };
            if let Some(report) = report {
                last = report.result.map_err(|e| *e.error)?.text;
            }
            let stop = parent
                .dispatch(HookEvent::SubagentStop {
                    stop_hook_active: continuation > 0,
                    agent_id: id.clone(),
                    agent_transcript_path: child
                        .context()
                        .transcript_path
                        .map(|p| p.to_string_lossy().into_owned())
                        .unwrap_or_default(),
                    agent_type: "worker".into(),
                    last_assistant_message: Some(last.clone()),
                })
                .await?;
            parent.consume_hook(&stop, false)?;
            if stop.common.blocking_errors.is_empty() {
                child.close(Duration::from_secs(10)).await?;
                return Ok(json!({"agent_id":id,"text":last}));
            }
            if continuation == 3 {
                child.close(Duration::from_secs(10)).await?;
                return Err(error("subagent", "SubagentStop continuation limit"));
            }
            child
                .submit(In::user_text(
                    stop.common
                        .blocking_errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
                .map_err(|e| error("subagent", e))?;
        }
        Err(error("subagent", "unreachable continuation state"))
    }
}
struct ChildCleanup(Arc<SessionHost>);
impl Drop for ChildCleanup {
    fn drop(&mut self) {
        if self.0.status() == SessionStatus::Closed {
            return;
        }
        self.0.interrupt();
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let child = self.0.clone();
            runtime.spawn(async move {
                let _ = child.close(Duration::from_secs(10)).await;
            });
        }
    }
}
impl ToolHandler for SubagentTool {
    fn name(&self) -> &str {
        "subagent"
    }
    fn definition(&self) -> Tool {
        Tool::new("subagent").with_schema(json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        SecurityContext {
            action: "subagent".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: true,
        }
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let prompt = input["prompt"]
                .as_str()
                .ok_or_else(|| error("subagent", "prompt required"))?;
            self.run_child(tc, prompt.into()).await
        })
    }
}
