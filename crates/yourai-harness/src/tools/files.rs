use crate::tools::*;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read as IoRead, Write as IoWrite},
    sync::{Mutex, OnceLock, Weak},
};
use tokio::sync::Mutex as AsyncMutex;
use tokio_util::sync::CancellationToken;

type FileLocks = Mutex<HashMap<PathBuf, Weak<AsyncMutex<()>>>>;
fn file_lock(path: &Path) -> Arc<AsyncMutex<()>> {
    static LOCKS: OnceLock<FileLocks> = OnceLock::new();
    let mut locks = LOCKS.get_or_init(Mutex::default).lock().unwrap();
    locks.retain(|_, v| v.strong_count() > 0);
    if let Some(lock) = locks.get(path).and_then(Weak::upgrade) {
        return lock;
    }
    let lock = Arc::new(AsyncMutex::new(()));
    locks.insert(path.into(), Arc::downgrade(&lock));
    lock
}

/// File I/O and per-line processing must not block the async executor
/// (multi-session starvation); run them on the blocking pool.
async fn blocking<T>(
    work: impl FnOnce() -> Result<T, YourAiError> + Send + 'static,
) -> Result<T, YourAiError>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(ErrorKind::Provider {
            name: "files",
            message: format!("blocking task failed: {e}"),
        }
        .into()),
    }
}

/// The blocking mutation owns its lock until the actual I/O finishes, even
/// when a timeout or cancellation drops the async waiter.
async fn mutate<T: Send + 'static>(
    path: PathBuf,
    cancel: CancellationToken,
    work: impl FnOnce() -> Result<T, YourAiError> + Send + 'static,
) -> Result<T, YourAiError> {
    let lock = file_lock(&path);
    let guard = tokio::select! {
        biased;
        _ = cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
        guard = lock.lock_owned() => guard,
    };
    blocking(move || {
        let _guard = guard;
        if cancel.is_cancelled() {
            return Err(AbortReason::Cancelled.into());
        }
        work()
    })
    .await
}

fn text(path: &Path, name: &str) -> Result<String, YourAiError> {
    let meta = fs::metadata(path).map_err(|e| error(name, e))?;
    if !meta.is_file() {
        return Err(error(name, "only regular text files are supported"));
    }
    if meta.len() > MAX_FILE_BYTES as u64 {
        return Err(error(name, "file exceeds 16 MiB; query it with shell"));
    }
    let file = fs::File::open(path).map_err(|e| error(name, e))?;
    let mut bytes = Vec::new();
    file.take(MAX_FILE_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|e| error(name, e))?;
    if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) || bytes.starts_with(b"%PDF-") {
        return Err(error(name, "oversized or binary file is unsupported"));
    }
    String::from_utf8(bytes).map_err(|_| error(name, "only UTF-8 text is supported"))
}
fn target(cwd: &Path, raw: &str, name: &str) -> Result<PathBuf, YourAiError> {
    if raw.is_empty() {
        return Err(error(name, "path must not be empty"));
    }
    let raw = if Path::new(raw).is_absolute() {
        PathBuf::from(raw)
    } else {
        cwd.join(raw)
    };
    // Check the final entry before canonicalization; replacing a symlink is ambiguous.
    match fs::symlink_metadata(&raw) {
        Ok(m) if m.file_type().is_symlink() => {
            return Err(error(name, "write/edit of a symlink is unsupported"))
        }
        Ok(m) if !m.is_file() => return Err(error(name, "target must be a regular file")),
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(error(name, e)),
    }
    resolve_path(cwd, &raw).map_err(|e| error(name, e))
}
fn save(
    path: &Path,
    content: &str,
    cancel: &CancellationToken,
    name: &str,
) -> Result<bool, YourAiError> {
    if content.len() > MAX_FILE_BYTES {
        return Err(error(name, "content exceeds 16 MiB"));
    }
    if cancel.is_cancelled() {
        return Err(AbortReason::Cancelled.into());
    }
    let previous = match fs::symlink_metadata(path) {
        Ok(m) if m.file_type().is_symlink() || !m.is_file() => {
            return Err(error(name, "target changed or is not a regular file"))
        }
        Ok(m) => Some(m),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => None,
        Err(e) => return Err(error(name, e)),
    };
    let parent = path
        .parent()
        .ok_or_else(|| error(name, "missing parent directory"))?;
    fs::create_dir_all(parent).map_err(|e| error(name, e))?;
    let mut tmp = tempfile::NamedTempFile::new_in(parent).map_err(|e| error(name, e))?;
    tmp.write_all(content.as_bytes())
        .map_err(|e| error(name, e))?;
    if let Some(meta) = &previous {
        tmp.as_file()
            .set_permissions(meta.permissions())
            .map_err(|e| error(name, e))?;
    }
    tmp.as_file().sync_all().map_err(|e| error(name, e))?;
    if cancel.is_cancelled() {
        return Err(AbortReason::Cancelled.into());
    }
    tmp.persist(path).map_err(|e| error(name, e))?;
    Ok(previous.is_none())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ReadInput {
    path: String,
    #[serde(default = "one")]
    offset: usize,
    #[serde(default = "page")]
    limit: usize,
}
fn one() -> usize {
    1
}
fn page() -> usize {
    200
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct WriteInput {
    path: String,
    content: String,
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct EditInput {
    path: String,
    old_text: String,
    new_text: String,
}

pub struct Read {
    cwd: PathBuf,
}
pub struct Write {
    cwd: PathBuf,
}
pub struct Edit {
    cwd: PathBuf,
}
macro_rules! constructor {
    ($t:ty) => {
        impl $t {
            pub fn new(cwd: PathBuf) -> Self {
                Self { cwd }
            }
        }
    };
}
constructor!(Read);
constructor!(Write);
constructor!(Edit);
impl ToolProvider for Read {
    fn definition(&self) -> ToolDefinition {
        schema("read","Read a UTF-8 text file with line numbers. offset is 1-based; use next_offset for subsequent pages. No images or binary files.",json!({"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":2000}}), &["path"])
    }
    fn security_context(&self, input: &Value, cwd: Option<&Path>) -> SecurityContext {
        security("read", cwd.unwrap_or(&self.cwd), input, false)
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: ReadInput = serde_json::from_value(input).map_err(|e| error("read", e))?;
            if i.path.is_empty() || i.offset == 0 || !(1..=2000).contains(&i.limit) {
                return Err(error("read", "invalid path, offset or limit"));
            }
            let cwd = tc.cwd.unwrap_or(&self.cwd).to_owned();
            let raw = i.path.clone();
            let tool = "read".to_owned();
            let path =
                blocking(move || resolve_path(&cwd, Path::new(&raw)).map_err(|e| error(&tool, e)))
                    .await?;
            let path_str = utf8_path(&path, "read")?.to_owned();
            file_permission(&tc, &path, false, "read").await?;
            let tool = "read".to_owned();
            let path = path.clone();
            let cancel = tc.cancel.clone();
            let offset = i.offset;
            let limit = i.limit;
            blocking(move || {
                let content = text(&path, &tool)?;
                let lines: Vec<_> = content.split_inclusive('\n').collect();
                if offset > lines.len() && !(lines.is_empty() && offset == 1) {
                    return Err(error(&tool, "offset is past end of file"));
                }
                let start = offset - 1;
                let mut end = start;
                let mut body = String::new();
                let mut clipped = false;
                for line in lines.iter().skip(start).take(limit) {
                    let (text, newline) = line
                        .strip_suffix('\n')
                        .map_or((*line, ""), |text| (text, "\n"));
                    let cut = text.char_indices().nth(2000).map(|(index, _)| index);
                    let text = match cut {
                        Some(index) => {
                            format!("{}... (line truncated to 2000 chars)", &text[..index])
                        }
                        None => text.to_owned(),
                    };
                    let numbered = format!("{}|{}{}", end + 1, text, newline);
                    if body.len() + numbered.len() > super::output::MAX_BYTES {
                        break;
                    }
                    clipped |= cut.is_some();
                    body.push_str(&numbered);
                    end += 1;
                }
                if cancel.is_cancelled() {
                    return Err(AbortReason::Cancelled.into());
                }
                Ok(json!({"ok":true,"path":path_str,"offset":offset,"next_offset":(end<lines.len()).then_some(end+1),"content":body,"lines_clipped":clipped}))
            })
            .await
        })
    }
}
impl ToolProvider for Write {
    fn definition(&self) -> ToolDefinition {
        schema("write","Create a UTF-8 file or OVERWRITE its entire content. Creates parent directories. Use edit for targeted changes.",json!({"path":{"type":"string","minLength":1},"content":{"type":"string"}}), &["path","content"])
    }
    fn security_context(&self, input: &Value, cwd: Option<&Path>) -> SecurityContext {
        security("write", cwd.unwrap_or(&self.cwd), input, true)
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: WriteInput = serde_json::from_value(input).map_err(|e| error("write", e))?;
            let cwd = tc.cwd.unwrap_or(&self.cwd).to_owned();
            let raw = i.path.clone();
            let tool = "write".to_owned();
            let path = blocking(move || target(&cwd, &raw, &tool)).await?;
            let path_str = utf8_path(&path, "write")?.to_owned();
            file_permission(&tc, &path, true, "write").await?;
            let tool = "write".to_owned();
            let content = i.content.clone();
            let cancel = tc.cancel.clone();
            let bytes = content.len();
            let created = mutate(path.clone(), cancel.clone(), move || {
                save(&path, &content, &cancel, &tool)
            })
            .await?;
            Ok(json!({"ok":true,"path":path_str,"created":created,"bytes_written":bytes}))
        })
    }
}
impl ToolProvider for Edit {
    fn definition(&self) -> ToolDefinition {
        schema("edit","Replace one exact, unique occurrence of old_text in an existing UTF-8 file. Include enough context to make the match unique. No fuzzy matching; new_text may be empty to delete text. Returns a diff.",json!({"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}}), &["path","old_text","new_text"])
    }
    fn security_context(&self, input: &Value, cwd: Option<&Path>) -> SecurityContext {
        security("edit", cwd.unwrap_or(&self.cwd), input, true)
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: EditInput = serde_json::from_value(input).map_err(|e| error("edit", e))?;
            if i.old_text.is_empty() {
                return Err(error("edit", "old_text must be nonempty"));
            }
            let cwd = tc.cwd.unwrap_or(&self.cwd).to_owned();
            let raw = i.path.clone();
            let tool = "edit".to_owned();
            let path = blocking(move || target(&cwd, &raw, &tool)).await?;
            let path_str = utf8_path(&path, "edit")?.to_owned();
            file_permission(&tc, &path, true, "edit").await?;
            let tool = "edit".to_owned();
            let old_text = i.old_text.clone();
            let new_text = i.new_text.clone();
            let cancel = tc.cancel.clone();
            mutate(path.clone(), cancel.clone(), move || {
                let before = text(&path, &tool)?;
                let Some(start) = before.find(&old_text) else {
                    return Err(error(&tool, "old_text not found; read the file again"));
                };
                let next = start + before[start..].chars().next().unwrap().len_utf8();
                if before[next..].contains(&old_text) {
                    return Err(error(
                        &tool,
                        "old_text matches more than once; include more context",
                    ));
                }
                let new_len = before.len() - old_text.len() + new_text.len();
                if new_len > MAX_FILE_BYTES {
                    return Err(error(&tool, "edited file exceeds 16 MiB"));
                }
                let after = before.replacen(&old_text, &new_text, 1);
                let changed = before != after;
                let diff = similar::TextDiff::configure()
                    .timeout(std::time::Duration::from_millis(200))
                    .diff_lines(&before, &after)
                    .unified_diff()
                    .header(&path_str, &path_str)
                    .to_string();
                if changed {
                    save(&path, &after, &cancel, &tool)?;
                } else if cancel.is_cancelled() {
                    return Err(AbortReason::Cancelled.into());
                }
                Ok(json!({"ok":true,"path":path_str,"changed":changed,"diff":diff}))
            })
            .await
        })
    }
}

#[cfg(test)]
mod mutation_tests {
    use super::*;

    #[tokio::test]
    async fn dropped_waiter_keeps_lock_until_blocking_write_finishes() {
        let dir = tempfile::TempDir::new().unwrap();
        let path = dir.path().join("shared.txt");
        let (started, ready) = tokio::sync::oneshot::channel();
        let (release, wait) = std::sync::mpsc::channel();
        let first_path = path.clone();
        let first = tokio::spawn(mutate(path.clone(), CancellationToken::new(), move || {
            let _ = started.send(());
            wait.recv_timeout(std::time::Duration::from_secs(5))
                .unwrap();
            fs::write(first_path, "first").map_err(|error| super::error("write", error))
        }));
        ready.await.unwrap();
        first.abort();
        assert!(first.await.unwrap_err().is_cancelled());
        // Capture the assertion before releasing the worker, so even a failure
        // does not strand a blocking thread or make the test hang at shutdown.
        let still_locked = file_lock(&path).try_lock().is_err();
        let next_path = path.clone();
        let second = tokio::spawn(mutate(path.clone(), CancellationToken::new(), move || {
            fs::write(next_path, "second").map_err(|error| super::error("write", error))
        }));
        release.send(()).unwrap();
        second.await.unwrap().unwrap();
        assert!(
            still_locked,
            "lock released while detached write was still running"
        );
        assert_eq!(fs::read_to_string(path).unwrap(), "second");
    }
}
