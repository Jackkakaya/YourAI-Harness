//! Child lifecycle wrapper; tool backend invokes this operation without dispatching hooks.
use super::backend::ChildCleanup;
use super::*;

impl SubagentTool {
    pub(super) async fn exec_child(
        &self,
        tc: ToolContext<'_>,
        prompt: String,
    ) -> Result<Value, YourAiError> {
        let plan = self.run_prepare().await?;
        let parent = &plan.parent;
        let id = plan.id.as_str().to_owned();
        let start = parent
            .dispatch(HookEvent::SubagentStart {
                agent_id: id.clone(),
                agent_type: "worker".into(),
            })
            .await?;
        parent.consume_hook_async(&start, true).await?;
        let child = self.run_open_child(&plan, &tc).await?;
        let _cleanup = ChildCleanup {
            children: self.children.clone(),
            id: id.clone(),
            child: child.clone(),
        };
        child
            .submit_async(In::user_text(prompt))
            .await
            .map_err(|e| error("subagent", e))?;
        let mut last = String::new();
        for continuation in 0..=3 {
            if let Some(text) = self.run_child_turn(&child, &tc, &id).await? {
                last = text;
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
            parent.consume_hook_async(&stop, false).await?;
            if stop.common.blocking_errors.is_empty() {
                child.close(None).await?;
                return Ok(json!({"agent_id":id,"text":last}));
            }
            if continuation == 3 {
                child.close(None).await?;
                return Err(error("subagent", "SubagentStop continuation limit"));
            }
            child
                .submit_async(In::user_text(
                    stop.common
                        .blocking_errors
                        .iter()
                        .map(|e| e.message.clone())
                        .collect::<Vec<_>>()
                        .join("\n"),
                ))
                .await
                .map_err(|e| error("subagent", e))?;
        }
        Err(error("subagent", "unreachable continuation state"))
    }
}
