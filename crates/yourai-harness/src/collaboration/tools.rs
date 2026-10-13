//! JSON protocol adapters; domain operations and lifecycle stay in core.
use crate::error;
use serde_json::{json, Value};
use std::{path::Path, sync::Arc};
use yourai_core::prelude::*;
pub struct TaskTool(pub Arc<TaskManager>);
pub struct SubagentTool(pub Arc<yourai_core::subagent::Subagent>);
impl ToolProvider for TaskTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("tasks")
            .with_description("Track coding TODOs. Create a task with a subject, list tasks, and mark a task complete by its id only when the work is done.")
            .with_schema(json!({
                "type":"object",
                "properties":{
                    "action":{"enum":["list","create","complete","idle"]},
                    "subject":{"type":"string"},
                    "id":{"type":"string"},
                    "owner":{"type":"string"}
                },
                "required":["action"]
            }))
    }
    fn security_context(&self, input: &Value, _cwd: Option<&Path>) -> SecurityContext {
        SecurityContext {
            action: "tasks".into(),
            input: input.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            if tc.cancel.is_cancelled() {
                return Err(AbortReason::Cancelled.into());
            }
            match input["action"].as_str() {
                Some("list") => Ok(json!(self.0.list())),
                Some("create") => Ok(json!(
                    self.0
                        .create(
                            input["subject"]
                                .as_str()
                                .ok_or_else(|| error("tasks", "subject required"))?
                                .into(),
                            None,
                            input["owner"].as_str().map(str::to_owned),
                        )
                        .await?
                )),
                Some("complete") => {
                    self.0
                        .complete(
                            input["id"]
                                .as_str()
                                .ok_or_else(|| error("tasks", "id required"))?,
                        )
                        .await?;
                    Ok(json!({"completed":true}))
                }
                Some("idle") => {
                    self.0
                        .idle(
                            input["owner"]
                                .as_str()
                                .ok_or_else(|| error("tasks", "owner required"))?,
                        )
                        .await?;
                    Ok(json!({"idle":true}))
                }
                _ => Err(error("tasks", "unknown action")),
            }
        })
    }
}
impl ToolProvider for SubagentTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition::new("subagent").with_schema(json!({"type":"object","properties":{"prompt":{"type":"string"}},"required":["prompt"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value, _cwd: Option<&std::path::Path>) -> SecurityContext {
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
                .ok_or_else(|| crate::error("subagent", "prompt required"))?;
            let (id, text) = self.0.exec(tc, prompt.into()).await?;
            Ok(json!({"agent_id": id.to_string(), "text": text}))
        })
    }
}
