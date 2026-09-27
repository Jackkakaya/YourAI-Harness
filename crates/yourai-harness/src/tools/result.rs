//! Session-scoped access to the original result, including compacted history.
use crate::error;
use serde_json::{json, Value};
use std::sync::Arc;
use yourai_core::prelude::*;
pub struct ReadToolResult {
    pub session: SessionId,
    pub store: Arc<dyn SessionManager>,
    pub max_chars: usize,
}
impl ToolHandler for ReadToolResult {
    fn name(&self) -> &str {
        "read_tool_result"
    }
    fn definition(&self) -> Tool {
        Tool::new(self.name()).with_schema(json!({"type":"object","properties":{"call_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1}},"required":["call_id"],"additionalProperties":false}))
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        SecurityContext {
            action: self.name().into(),
            input: input.clone(),
            is_destructive: false,
            is_network: false,
        }
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let call = input
                .get("call_id")
                .and_then(Value::as_str)
                .ok_or_else(|| error("read_tool_result", "call_id required"))?;
            let offset = input
                .get("offset")
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| error("read_tool_result", "offset must be nonnegative"))
                })
                .transpose()?
                .unwrap_or(0);
            let limit = input
                .get("limit")
                .map(|v| {
                    v.as_u64()
                        .ok_or_else(|| error("read_tool_result", "limit must be positive"))
                })
                .transpose()?
                .unwrap_or(2000);
            if limit == 0 {
                return Err(error("read_tool_result", "limit must be positive"));
            }
            let mut after = 0;
            loop {
                let page = tokio::select! { biased; _ = tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()), r = self.store.read_messages(&self.session, MessageQuery { after, limit:100, active_only:false }) => r? };
                for m in page.messages {
                    for r in m.message.content.tool_responses() {
                        if r.call_id != call {
                            continue;
                        }
                        let total = r.content.chars().count();
                        let start = usize::try_from(offset).unwrap_or(usize::MAX).min(total);
                        let mut size = usize::try_from(limit)
                            .unwrap_or(usize::MAX)
                            .min(self.max_chars)
                            .min(total - start);
                        loop {
                            let text: String = r.content.chars().skip(start).take(size).collect();
                            let next = (start + size < total).then_some(start + size);
                            let result = json!({"ok":true,"call_id":call,"offset":start,"next":next,"total":total,"content":text});
                            if result.to_string().chars().count() <= self.max_chars {
                                return Ok(result);
                            }
                            if size == 0 {
                                return Err(error(
                                    "read_tool_result",
                                    "output limit cannot fit result metadata",
                                ));
                            }
                            size /= 2;
                        }
                    }
                }
                match page.next {
                    Some(next) => after = next,
                    None => {
                        return Err(error(
                            "read_tool_result",
                            "no result for call_id in this session",
                        ))
                    }
                }
            }
        })
    }
}
