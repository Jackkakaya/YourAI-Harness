use super::*;
use genai::chat::ContentPart;

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
                        normalize_output_refs(response);
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
