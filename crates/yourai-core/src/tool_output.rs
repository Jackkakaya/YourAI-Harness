//! Managed tool output, separate from the context's smaller request projection.
use crate::prelude::*;
use serde_json::{json, Value};
use std::{
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};
fn error(name: &str, message: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Tool {
        name: name.into(),
        message: message.to_string(),
    }
    .into()
}
pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
const CHUNK_BYTES: usize = 128 * 1024;
const RETENTION_MS: i64 = 7 * 24 * 3600 * 1000;

/// Session-scoped database output access; references are independent of worker paths.
pub struct ToolOutputStore {
    store: Arc<dyn SessionStore>,
    session: SessionId,
}
impl std::fmt::Debug for ToolOutputStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolOutputStore")
            .field("session", &self.session)
            .finish_non_exhaustive()
    }
}
impl ToolOutputStore {
    pub fn new(store: Arc<dyn SessionStore>, session: SessionId) -> Arc<Self> {
        let output = Arc::new(Self { store, session });
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&output);
            runtime.spawn(async move {
                loop {
                    let Some(output) = weak.upgrade() else {
                        break;
                    };
                    if output.cleanup().await.is_err() {
                        break;
                    }
                    drop(output);
                    tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
                }
            });
        }
        output
    }
    /// Translate the previous managed filename into its stable database identity.
    pub fn legacy_reference(path: &str) -> Option<String> {
        let path = std::path::Path::new(path);
        if path.parent()?.file_name()?.to_str()? != "tool-output" {
            return None;
        }
        let id = path
            .file_name()?
            .to_str()?
            .strip_prefix("tool_")?
            .strip_suffix(".txt")?;
        uuid::Uuid::parse_str(id).ok()?;
        Some(format!("tool-output://{id}"))
    }
    pub fn create(&self) -> String {
        format!("tool-output://{}", uuid::Uuid::new_v4())
    }
    fn id(reference: &str) -> Result<&str, YourAiError> {
        let id = reference
            .strip_prefix("tool-output://")
            .ok_or_else(|| error("tool-output", "invalid output reference"))?;
        uuid::Uuid::parse_str(id).map_err(|_| error("tool-output", "invalid output identity"))?;
        Ok(id)
    }
    pub async fn append(
        &self,
        reference: &str,
        mut offset: u64,
        bytes: &[u8],
    ) -> Result<(), YourAiError> {
        let id = Self::id(reference)?;
        for chunk in bytes.chunks(CHUNK_BYTES) {
            self.store
                .append_output(&self.session, id, offset, chunk.to_vec())
                .await?;
            offset += chunk.len() as u64;
        }
        Ok(())
    }
    pub async fn read(
        &self,
        reference: &str,
        offset: u64,
        limit: usize,
    ) -> Result<Vec<u8>, YourAiError> {
        if limit == 0 || limit > 1024 * 1024 {
            return Err(error(
                "tool-output",
                "output read limit must be 1..=1048576 bytes",
            ));
        }
        // Ordinary tool results already live in immutable message rows. Reuse
        // their session-local sequence instead of duplicating them into BLOBs.
        if let Some(sequence) = reference.strip_prefix("tool-output://message/") {
            let seq = sequence
                .parse::<i64>()
                .ok()
                .filter(|seq| *seq > 0)
                .ok_or_else(|| error("tool-output", "invalid message output reference"))?;
            let page = self
                .store
                .read_messages(
                    &self.session,
                    MessageQuery {
                        after: seq - 1,
                        active_only: false,
                        limit: 1,
                    },
                )
                .await?;
            let record = page
                .messages
                .first()
                .filter(|m| m.seq == seq)
                .ok_or_else(|| error("tool-output", "unknown message output"))?;
            let responses = record.message.content.tool_responses();
            if responses.len() != 1 {
                return Err(error("tool-output", "message is not a single tool result"));
            }
            let bytes = responses[0].content.as_bytes();
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(bytes.len());
            let end = start.saturating_add(limit).min(bytes.len());
            return Ok(bytes[start..end].to_vec());
        }
        self.store
            .read_output(&self.session, Self::id(reference)?, offset, limit)
            .await
    }
    pub async fn cleanup(&self) -> Result<(), YourAiError> {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map_err(|e| error("tool-output", e))?
            .as_millis() as i64;
        self.store
            .cleanup_outputs(&self.session, now - RETENTION_MS)
            .await
    }
    /// Called before the after hook; the hook receives this bounded result.
    pub(crate) async fn bound(&self, value: &Value) -> Result<Value, YourAiError> {
        let text = serde_json::to_string_pretty(value).map_err(|e| error("tool-output", e))?;
        if text.len() <= MAX_BYTES
            && text.lines().count() <= MAX_LINES
            && content_lines(value) <= MAX_LINES
        {
            return Ok(value.clone());
        }
        let reference = self.create();
        self.append(&reference, 0, text.as_bytes()).await?;
        let mut refs = value
            .get("output_refs")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        refs.push(json!(reference));
        Ok(json!({"truncated":true,"output_refs":refs,
            "status":{"ok":value.get("ok"),"error":value.get("error"),"exit_code":value.get("exit_code"),"termination":value.get("termination")},
            "content":preview(&text,&reference,MAX_LINES,MAX_BYTES)}))
    }
}
fn content_lines(value: &Value) -> usize {
    match value {
        Value::String(s) => s.bytes().filter(|b| *b == b'\n').take(MAX_LINES).count() + 1,
        Value::Array(v) => v.iter().map(content_lines).fold(0, usize::saturating_add),
        Value::Object(v) => v.values().map(content_lines).fold(0, usize::saturating_add),
        _ => 0,
    }
}

/// Head/tail sampling with UTF-8 safe bounds, including the retrieval marker.
pub fn preview(text: &str, path: &str, lines: usize, bytes: usize) -> String {
    let parts: Vec<_> = text.split('\n').collect();
    let budget = lines.saturating_sub(4);
    if budget > 0 && parts.len() > budget {
        return preview_parts(
            &parts[..budget.div_ceil(2)].join("\n"),
            &parts[parts.len() - budget / 2..].join("\n"),
            path,
            lines,
            bytes,
        );
    }
    let head = prefix(text, text.len() / 2);
    preview_parts(head, &text[head.len()..], path, lines, bytes)
}

/// Format already-separated ranges. Never join across an omitted middle before
/// inserting the marker: the retained ranges may have very different lengths.
pub fn preview_parts(head: &str, tail: &str, path: &str, lines: usize, bytes: usize) -> String {
    let marker =
        format!("... output truncated; stored as {path}; use read with byte_offset/byte_limit ...");
    if lines == 0 {
        return String::new();
    }
    let marker_bytes = marker.len() + 4;
    if lines <= 4 || bytes <= marker_bytes {
        return prefix(&marker, bytes)
            .split('\n')
            .take(lines)
            .collect::<Vec<_>>()
            .join("\n");
    }
    let line_budget = lines - 4;
    let head = head
        .split('\n')
        .take(line_budget.div_ceil(2))
        .collect::<Vec<_>>()
        .join("\n");
    let tail = tail
        .rsplit('\n')
        .take(line_budget / 2)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect::<Vec<_>>()
        .join("\n");
    let budget = bytes - marker_bytes;
    let head = prefix(&head, budget.div_ceil(2));
    let mut start = tail.len().saturating_sub(budget / 2);
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    format!("{head}\n\n{marker}\n\n{}", &tail[start..])
}

fn prefix(text: &str, bytes: usize) -> &str {
    let mut end = bytes.min(text.len());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn preview_places_marker_between_asymmetric_line_samples() {
        let text = format!(
            "HEAD{}\nhead-end\nMIDDLE\nMIDDLE\ntail-start\nTAIL",
            "x".repeat(4000)
        );
        let out = preview(&text, "/output/file", 8, 1000);
        let (head, tail) = out.split_once("output truncated").unwrap();
        assert!(head.starts_with("HEAD"));
        assert!(!head.contains("tail-start"));
        assert!(tail.ends_with("tail-start\nTAIL"));
        assert!(!out.contains("MIDDLE"));
        assert!(out.len() <= 1000);
        assert!(out.lines().count() <= 8);
        for lines in 0..6 {
            for bytes in 0..120 {
                let out = preview(&text, "/output/file", lines, bytes);
                assert!(out.len() <= bytes);
                assert!(out.lines().count() <= lines);
            }
        }
    }

    #[test]
    fn preview_bounds_unicode_and_long_markers_and_keeps_tail() {
        let text = format!("HEAD\n{}\nTAIL", "界\n".repeat(3000));
        for bytes in [32, 512, MAX_BYTES] {
            let out = preview(&text, "/output/file", 20, bytes);
            assert!(out.len() <= bytes);
            assert!(out.lines().count() <= 20);
            if bytes >= 512 {
                assert!(out.starts_with("HEAD"));
                assert!(out.ends_with("TAIL"));
            }
        }
    }
}
