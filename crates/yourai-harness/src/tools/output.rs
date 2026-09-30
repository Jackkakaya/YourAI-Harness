//! Managed tool output, separate from the context's smaller request projection.
use super::*;
use std::time::{Duration, SystemTime};
use tokio::io::AsyncWriteExt;

pub(crate) const MAX_LINES: usize = 2000;
pub(crate) const MAX_BYTES: usize = 50 * 1024;
const RETENTION: Duration = Duration::from_secs(7 * 24 * 3600);

/// Immutable storage destination. Sharing a store never changes another session's root.
#[derive(Debug)]
pub struct ToolOutputStore {
    directory: PathBuf,
}
impl ToolOutputStore {
    pub fn open(directory: PathBuf) -> Result<Arc<Self>, YourAiError> {
        std::fs::create_dir_all(&directory).map_err(|e| error("tool-output", e))?;
        let directory = std::fs::canonicalize(directory).map_err(|e| error("tool-output", e))?;
        utf8_path(&directory, "tool-output")?;
        let store = Arc::new(Self { directory });
        // The worker owns only a Weak between scans; it cannot keep a closed harness alive.
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&store);
            runtime.spawn(async move {
                while let Some(store) = weak.upgrade() {
                    let _ = store.cleanup().await;
                    drop(store);
                    tokio::time::sleep(Duration::from_secs(3600)).await;
                }
            });
        }
        Ok(store)
    }

    pub(crate) async fn create(&self) -> Result<(PathBuf, tokio::fs::File), YourAiError> {
        let path = self
            .directory
            .join(format!("tool_{}.txt", uuid::Uuid::new_v4()));
        let mut options = tokio::fs::OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let file = options
            .open(&path)
            .await
            .map_err(|e| error("tool-output", e))?;
        Ok((path, file))
    }

    pub async fn cleanup(&self) -> Result<(), YourAiError> {
        let cutoff = SystemTime::now()
            .checked_sub(RETENTION)
            .unwrap_or(SystemTime::UNIX_EPOCH);
        let mut entries = tokio::fs::read_dir(&self.directory)
            .await
            .map_err(|e| error("tool-output", e))?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| error("tool-output", e))?
        {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("tool_") || !name.ends_with(".txt") {
                continue;
            }
            let Ok(metadata) = tokio::fs::symlink_metadata(entry.path()).await else {
                continue;
            };
            if metadata.is_file() && metadata.modified().is_ok_and(|time| time < cutoff) {
                let _ = tokio::fs::remove_file(entry.path()).await;
            }
        }
        Ok(())
    }

    /// Called once after tool hooks, so custom and MCP results get the same treatment.
    pub(crate) async fn bound(&self, value: &Value) -> Result<Value, YourAiError> {
        let text = serde_json::to_string_pretty(value).map_err(|e| error("tool-output", e))?;
        if text.len() <= MAX_BYTES
            && text.lines().count() <= MAX_LINES
            && content_lines(value) <= MAX_LINES
        {
            return Ok(value.clone());
        }
        let (path, mut file) = self.create().await?;
        file.write_all(text.as_bytes())
            .await
            .map_err(|e| error("tool-output", e))?;
        file.flush().await.map_err(|e| error("tool-output", e))?;
        let path = utf8_path(&path, "tool-output")?;
        let mut paths = value
            .get("output_paths")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        paths.push(json!(path));
        Ok(json!({
            "truncated":true, "output_paths":paths,
            "status":{"ok":value.get("ok"),"error":value.get("error"),"exit_code":value.get("exit_code"),"termination":value.get("termination")},
            "content":preview(&text, path, MAX_LINES, MAX_BYTES),
        }))
    }
}

// JSON escapes newlines inside strings; count their actual content as well.
fn content_lines(value: &Value) -> usize {
    match value {
        Value::String(text) => {
            text.bytes()
                .filter(|byte| *byte == b'\n')
                .take(MAX_LINES)
                .count()
                + 1
        }
        Value::Array(values) => values
            .iter()
            .map(content_lines)
            .fold(0, usize::saturating_add),
        Value::Object(values) => values
            .values()
            .map(content_lines)
            .fold(0, usize::saturating_add),
        _ => 0,
    }
}

/// Head/tail sampling with UTF-8 safe bounds, including the retrieval marker.
pub(crate) fn preview(text: &str, path: &str, lines: usize, bytes: usize) -> String {
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
pub(crate) fn preview_parts(
    head: &str,
    tail: &str,
    path: &str,
    lines: usize,
    bytes: usize,
) -> String {
    let marker = format!(
        "... output truncated; saved to {path}; use read with offset/limit or shell to search ..."
    );
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
    #[tokio::test]
    async fn stores_are_isolated_and_writes_never_overwrite() {
        let root = tempfile::tempdir().unwrap();
        let a = ToolOutputStore::open(root.path().join("a")).unwrap();
        let b = ToolOutputStore::open(root.path().join("b")).unwrap();
        let value = json!({"ok":true,"content":"界".repeat(30_000)});
        let lines = a
            .bound(&json!({"content":"x\n".repeat(2100)}))
            .await
            .unwrap();
        assert_eq!(lines["truncated"], true);
        let first = a.bound(&value).await.unwrap();
        let other = b.bound(&value).await.unwrap();
        let next = a.bound(&value).await.unwrap();
        let path = |v: &Value| PathBuf::from(v["output_paths"][0].as_str().unwrap());
        assert_ne!(path(&first), path(&next));
        assert_eq!(path(&first).parent(), path(&next).parent());
        assert_ne!(path(&first).parent(), path(&other).parent());
        let saved: Value =
            serde_json::from_slice(&tokio::fs::read(path(&first)).await.unwrap()).unwrap();
        assert_eq!(saved, value);
        assert!(first["content"].as_str().unwrap().len() <= MAX_BYTES);
        assert_eq!(first["status"]["ok"], true);
    }
    #[tokio::test]
    async fn cleanup_removes_only_expired_managed_regular_files() {
        let root = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::open(root.path().to_owned()).unwrap();
        for name in ["tool_old.txt", "unmanaged.txt", "tool_fresh.txt"] {
            std::fs::write(root.path().join(name), name).unwrap();
        }
        for name in ["tool_old.txt", "unmanaged.txt"] {
            std::fs::File::options()
                .write(true)
                .open(root.path().join(name))
                .unwrap()
                .set_times(std::fs::FileTimes::new().set_modified(SystemTime::UNIX_EPOCH))
                .unwrap();
        }
        store.cleanup().await.unwrap();
        assert!(!root.path().join("tool_old.txt").exists());
        assert!(root.path().join("unmanaged.txt").exists());
        assert!(root.path().join("tool_fresh.txt").exists());
    }
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
