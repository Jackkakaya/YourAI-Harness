mod backend;
mod operations;
use crate::{error, SessionHost};
use serde_json::{json, Value};
use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{Arc, Mutex, Weak},
};
use yourai_core::prelude::*;
/// Subagents are independent sessions. Parent owns cancellation and observes child output.
pub struct SubagentTool {
    host: Weak<SessionHost>,
    model: Arc<dyn ModelProvider>,
    tools: Option<Arc<dyn ToolRegistry>>,
    children: Arc<Mutex<HashMap<String, Arc<SessionHost>>>>,
    // Opening a catalog scans the directory and opens SQLite; reuse it across runs.
    catalog: Mutex<Option<(PathBuf, Arc<crate::SessionCatalog>)>>,
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
            children: Arc::new(Mutex::new(HashMap::new())),
            catalog: Mutex::new(None),
        })
    }
    pub fn child_ids(&self) -> Vec<String> {
        self.children.lock().unwrap().keys().cloned().collect()
    }
    pub async fn stop_all(&self) -> Result<(), YourAiError> {
        let children: Vec<_> = self.children.lock().unwrap().values().cloned().collect();
        for child in children {
            child.close(None).await?;
        }
        Ok(())
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
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let prompt = input["prompt"]
                .as_str()
                .ok_or_else(|| error("subagent", "prompt required"))?;
            self.exec_child(tc, prompt.into()).await
        })
    }
}
