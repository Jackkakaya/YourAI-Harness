use super::*;
use genai::chat::ContentPart;

/// Valid JSON envelope even when the original output was structured JSON.
pub(crate) fn preview(
    response: &ToolResponse,
    limit: usize,
    pruned: bool,
) -> Result<String, YourAiError> {
    if !pruned && response.content.chars().count() <= limit {
        return Ok(response.content.clone());
    }
    let parsed: Option<serde_json::Value> = serde_json::from_str(&response.content).ok();
    let status = parsed.as_ref().map(|v| v.get("status").cloned().unwrap_or_else(|| serde_json::json!({"ok":v.get("ok"),"error":v.get("error"),"exit_code":v.get("exit_code")})));
    let paths = parsed.as_ref().and_then(|v| v.get("output_paths"));
    let mut take = if pruned { 0 } else { limit / 2 };
    loop {
        let head: String = response.content.chars().take(take / 2).collect();
        let tail: String = response
            .content
            .chars()
            .rev()
            .take(take / 2)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        let s = serde_json::json!({"truncated":true,"pruned":pruned,"call_id":response.call_id,"status":status,"output_paths":paths,"head":head,"tail":tail}).to_string();
        if s.chars().count() <= limit {
            return Ok(s);
        }
        if take == 0 {
            return Err(error(
                "context",
                "tool output limit cannot fit status and retrieval pointer",
            ));
        }
        take /= 2;
    }
}
impl DefaultContext {
    pub(super) fn project_messages(
        &self,
        records: &[StoredMessage],
    ) -> Result<Vec<ChatMessage>, YourAiError> {
        let mut messages = vec![];
        let mut pending_media = vec![];
        let mut open = HashSet::new();
        let current = yourai_core::session::current_context_ids(records);
        for record in records
            .iter()
            .filter(|m| m.context_key.is_none() || current.contains(m.id.as_str()))
        {
            let mut message = record.model_message();
            let mut parts = message.content.into_parts();
            for part in &mut parts {
                match part {
                    ContentPart::ToolCall(call) => {
                        open.insert(call.call_id.clone());
                    }
                    ContentPart::ToolResponse(response) => {
                        open.remove(&response.call_id);
                        response.content = preview(
                            response,
                            self.policy.tool_output_chars,
                            record.tool_output_pruned_at.is_some(),
                        )?;
                    }
                    _ => {}
                }
            }
            if message.role == ChatRole::Tool {
                parts.retain(|part| {
                    if matches!(part, ContentPart::Binary(_) | ContentPart::Custom(_)) {
                        pending_media.push(part.clone());
                        false
                    } else {
                        true
                    }
                });
            }
            message.content = MessageContent::from_parts(parts);
            messages.push(message);
            if open.is_empty() && !pending_media.is_empty() {
                messages.push(ChatMessage::user(MessageContent::from_parts(
                    std::mem::take(&mut pending_media),
                )));
            }
        }
        if !pending_media.is_empty() {
            return Err(error("assets", "media tool batch has unresolved calls"));
        }
        Ok(messages)
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;
    #[test]
    fn projection_and_pruning_preserve_managed_paths_and_exit_status() {
        let call = ToolCall {
            call_id: "output-test".into(),
            fn_name: "shell".into(),
            fn_arguments: serde_json::json!({}),
            thought_signatures: None,
        };
        let value = serde_json::json!({"status":{"exit_code":7,"ok":false},"output_paths":["/tmp/tool_a.txt","/tmp/tool_b.txt"],"content":"abc".repeat(20_000)});
        let response = ToolResponse::from_tool_call(&call, value.to_string());
        for pruned in [false, true] {
            let result = preview(&response, 512, pruned).unwrap();
            assert!(result.chars().count() <= 512);
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(parsed["output_paths"], value["output_paths"]);
            assert_eq!(parsed["status"]["exit_code"], 7);
        }
    }
}
