use super::*;
use genai::chat::ContentPart;

/// Valid JSON envelope even when the original output was structured JSON.
pub(crate) fn preview(
    response: &ToolResponse,
    limit: usize,
    pruned: bool,
    archive: Option<&str>,
) -> Result<String, YourAiError> {
    if !pruned && response.content.chars().count() <= limit {
        return Ok(response.content.clone());
    }
    let parsed: Option<serde_json::Value> = serde_json::from_str(&response.content).ok();
    let status = parsed.as_ref().map(|v| {
        v.get("status").cloned().unwrap_or_else(|| {
            let mut status = serde_json::json!({"ok":v.get("ok"),"error":v.get("error"),"exit_code":v.get("exit_code")});
            for key in ["termination", "output_complete"] {
                if let Some(value) = v.get(key) {
                    status[key] = value.clone();
                }
            }
            status
        })
    });
    let mut paths = parsed
        .as_ref()
        .and_then(|v| v.get("output_refs").or_else(|| v.get("output_paths")))
        .cloned();
    if let Some(archive) = archive {
        let mut refs = paths
            .and_then(|v| v.as_array().cloned())
            .unwrap_or_default();
        let reference = serde_json::json!(archive);
        if !refs.contains(&reference) {
            refs.push(reference);
        }
        paths = Some(serde_json::json!(refs));
    }
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
        let s = serde_json::json!({"truncated":true,"pruned":pruned,"call_id":response.call_id,"status":status,"output_refs":paths,"head":head,"tail":tail}).to_string();
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
/// Normalize only managed legacy output references in the outgoing copy.
/// Stored message identities/content stay immutable for retry and history replay.
fn normalize_output_refs(response: &mut ToolResponse) {
    let Ok(mut value) = serde_json::from_str::<serde_json::Value>(&response.content) else {
        return;
    };
    let Some(map) = value.as_object_mut() else {
        return;
    };
    let mut substitutions = Vec::new();
    if let Some(paths) = map.get("output_paths").and_then(|v| v.as_array()) {
        let mut refs = Vec::new();
        let mut files = Vec::new();
        for path in paths {
            match path
                .as_str()
                .and_then(crate::tools::ToolOutputStore::legacy_reference)
            {
                Some(reference) => {
                    substitutions.push((path.as_str().unwrap().to_owned(), reference.clone()));
                    refs.push(serde_json::json!(reference));
                }
                None => files.push(path.clone()),
            }
        }
        if !refs.is_empty() {
            map.insert("output_refs".into(), serde_json::json!(refs));
            if files.is_empty() {
                map.remove("output_paths");
            } else {
                map.insert("output_paths".into(), serde_json::json!(files));
            }
        }
    }
    if substitutions.is_empty() {
        return;
    }
    for (old, new) in [("stdout_path", "stdout_ref"), ("stderr_path", "stderr_ref")] {
        if let Some(reference) = map
            .get(old)
            .and_then(|v| v.as_str())
            .and_then(crate::tools::ToolOutputStore::legacy_reference)
        {
            map.remove(old);
            map.insert(new.into(), serde_json::json!(reference));
        }
    }
    for key in ["content", "stdout", "stderr"] {
        if let Some(text) = map.get_mut(key).and_then(|v| v.as_str().map(str::to_owned)) {
            let text = substitutions
                .iter()
                .fold(text, |text, (old, new)| text.replace(old, new));
            map.insert(key.into(), serde_json::json!(text));
        }
    }
    response.content = value.to_string();
}
impl DefaultContext {
    pub(super) fn project_messages(
        &self,
        records: &[StoredMessage],
    ) -> Result<Vec<ChatMessage>, YourAiError> {
        let mut messages = vec![];
        let mut pending_media = vec![];
        let mut open = HashSet::new();
        // Only real user messages define turns: summaries, hooks, and runtime
        // notes must not make a current tool result appear old.
        let protect_from = records
            .iter()
            .enumerate()
            .rev()
            .filter(|(_, m)| m.message.role == ChatRole::User && !m.summary && !m.runtime_context)
            .nth(1)
            .map_or(0, |(i, _)| i);
        let mut candidates = Vec::new();
        let mut batch = Vec::new();
        let current = yourai_core::session::current_context_ids(records);
        for (index, record) in records
            .iter()
            .enumerate()
            .filter(|(_, m)| m.context_key.is_none() || current.contains(m.id.as_str()))
        {
            let mut message = record.model_message();
            let mut parts = message.content.into_parts();
            for part in &mut parts {
                match part {
                    ContentPart::ToolCall(call) => {
                        open.insert(call.call_id.clone());
                    }
                    ContentPart::ToolResponse(response) => {
                        let matched = open.remove(&response.call_id);
                        normalize_output_refs(response);
                        response.content = preview(
                            response,
                            self.policy.tool_output_chars,
                            record.tool_output_pruned_at.is_some(),
                            None,
                        )?;
                        if matched
                            && self.store.is_some()
                            && record.seq > 0
                            && index < protect_from
                            && !record.runtime_context
                            && !record.summary
                            && record.tool_output_pruned_at.is_none()
                            && self.policy.tool_prune_min_savings > 0
                        {
                            let reference = format!("tool-output://message/{}", record.seq);
                            let Ok(pruned) = preview(
                                response,
                                self.policy.tool_output_chars,
                                true,
                                Some(&reference),
                            ) else {
                                // Keep the normal projection if its status and
                                // retrieval references cannot fit a prune marker.
                                continue;
                            };
                            // Match the request estimator's serialized UTF-8 bytes / 3.
                            // Measure the already bounded projection, not the huge archive.
                            let size = serde_json::to_vec(&response.content)
                                .map_err(|e| error("context", e))?
                                .len() as u64;
                            let after = serde_json::to_vec(&pruned)
                                .map_err(|e| error("context", e))?
                                .len() as u64;
                            batch.push((
                                messages.len(),
                                response.call_id.clone(),
                                pruned,
                                size.div_ceil(3),
                                size.saturating_sub(after),
                            ));
                        }
                    }
                    _ => {}
                }
            }
            // Do not prune any results in an unresolved parallel tool batch.
            if open.is_empty() {
                candidates.append(&mut batch);
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
        let mut kept = 0u64;
        let mut savings_bytes = 0u64;
        let mut selected = Vec::new();
        for (message, call_id, pruned, size, saved) in candidates.into_iter().rev() {
            if kept < self.policy.tool_prune_keep_tokens {
                kept = kept.saturating_add(size);
            } else if saved > 0 {
                savings_bytes = savings_bytes.saturating_add(saved);
                selected.push((message, call_id, pruned));
            }
        }
        // Sum actual serialized savings before rounding. Per-result rounding
        // could overstate the net gain for many small outputs.
        if self.policy.tool_prune_min_savings > 0
            && savings_bytes / 3 >= self.policy.tool_prune_min_savings
        {
            for (message, call_id, pruned) in selected {
                let mut parts = messages[message].content.clone().into_parts();
                for part in &mut parts {
                    if let ContentPart::ToolResponse(response) = part {
                        if response.call_id == call_id {
                            response.content = pruned.clone();
                        }
                    }
                }
                messages[message].content = parts.into();
            }
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
            let result = preview(&response, 512, pruned, None).unwrap();
            assert!(result.chars().count() <= 512);
            let parsed: serde_json::Value = serde_json::from_str(&result).unwrap();
            assert_eq!(parsed["output_refs"], value["output_paths"]);
            assert_eq!(parsed["status"]["exit_code"], 7);
        }
    }
}
