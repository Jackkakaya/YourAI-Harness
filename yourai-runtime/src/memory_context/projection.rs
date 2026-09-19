use super::*;
use genai::chat::ContentPart;

fn estimate(request: &ChatRequest, model: &dyn ModelProvider) -> Result<u64, YourAiError> {
    let mut text = request.clone();
    let mut media = 0u64;
    for message in &mut text.messages {
        let mut parts = vec![];
        for part in message.content.clone().into_parts() {
            if matches!(part, ContentPart::Binary(_) | ContentPart::Custom(_)) {
                media = media
                    .checked_add(model.media_tokens(&part)?)
                    .ok_or_else(|| error("context", "media budget overflow"))?;
            } else {
                parts.push(part);
            }
        }
        message.content = MessageContent::from_parts(parts);
    }
    let tokens = serde_json::to_vec(&text)
        .map_err(|e| error("context", e))?
        .len()
        .div_ceil(3) as u64;
    tokens
        .checked_add(media)
        .ok_or_else(|| error("context", "request budget overflow"))
}
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
    let status = parsed.as_ref().map(|v| serde_json::json!({"ok":v.get("ok"),"error":v.get("error"),"exit_code":v.get("exit_code")}));
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
        let s = serde_json::json!({"truncated":true,"pruned":pruned,"call_id":response.call_id,"status":status,"head":head,"tail":tail,"read":"read_tool_result(call_id, offset, limit)"}).to_string();
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
impl MemoryContext {
    pub(super) fn estimate_raw(
        &self,
        request: &ChatRequest,
        execution: &ContextExecution,
    ) -> Result<u64, YourAiError> {
        estimate(request, execution.model.as_ref())
    }

    pub(super) fn estimate(
        &self,
        request: &ChatRequest,
        execution: &ContextExecution,
    ) -> Result<u64, YourAiError> {
        let raw = self.estimate_raw(request, execution)?;
        let view = self.view.lock().unwrap();
        if let Some(o) = &view.observation {
            let n = o.request.messages.len();
            if o.model == execution.model.model_iden()
                && n <= request.messages.len()
                && o.request.system == request.system
                && serde_json::to_value(&o.request.tools).ok()
                    == serde_json::to_value(&request.tools).ok()
                && serde_json::to_value(&o.request.messages).ok()
                    == serde_json::to_value(&request.messages[..n]).ok()
            {
                return Ok(o.input_tokens.saturating_add(
                    raw.saturating_sub(self.estimate_raw(&o.request, execution)?),
                ));
            }
        }
        Ok(raw)
    }

    pub(super) fn project(
        &self,
        records: &[StoredMessage],
        system: Option<&str>,
        tools: &[Tool],
    ) -> Result<ChatRequest, YourAiError> {
        let messages = self.project_messages(records, true)?;
        let mut request = ChatRequest::new(messages);
        request.system = system.map(str::to_owned);
        request.tools = Some(tools.to_vec());
        Ok(request)
    }
    pub(super) fn project_messages(
        &self,
        records: &[StoredMessage],
        limit_tools: bool,
    ) -> Result<Vec<ChatMessage>, YourAiError> {
        let mut messages = vec![];
        let mut pending_media = vec![];
        let mut open = HashSet::new();
        for record in records {
            let mut message = record.message.clone();
            let mut parts = message.content.into_parts();
            for part in &mut parts {
                match part {
                    ContentPart::ToolCall(call) => {
                        open.insert(call.call_id.clone());
                    }
                    ContentPart::ToolResponse(response) => {
                        open.remove(&response.call_id);
                        if limit_tools {
                            response.content = preview(
                                response,
                                self.services.policy.tool_output_chars,
                                record.tool_output_pruned_at.is_some(),
                            )?;
                        }
                    }
                    _ => {}
                }
            }
            if !limit_tools {
                parts = parts.into_iter().map(|part| match part {
                    ContentPart::Binary(binary) => {
                        let location=match binary.source { BinarySource::Url(url)=>Some(url), BinarySource::Base64(_)=>None };
                        ContentPart::from_text(format!("[Media omitted; contents not inspected] {}",serde_json::json!({"type":binary.content_type,"name":binary.name,"location":location})))
                    },
                    ContentPart::Custom(_) => ContentPart::from_text("[Custom media omitted; contents not inspected]"),
                    other => other,
                }).collect();
            } else if message.role == ChatRole::Tool {
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
