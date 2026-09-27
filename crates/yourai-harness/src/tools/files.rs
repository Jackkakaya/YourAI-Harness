use crate::tools::*;
use serde::Deserialize;
use std::{
    collections::HashMap,
    fs,
    io::{Read as IoRead, Write as IoWrite},
    sync::{Mutex, OnceLock, Weak},
};
use tokio::sync::Mutex as AsyncMutex;

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
fn save(path: &Path, content: &str, tc: &ToolContext<'_>, name: &str) -> Result<bool, YourAiError> {
    if content.len() > MAX_FILE_BYTES {
        return Err(error(name, "content exceeds 16 MiB"));
    }
    check_cancel(tc)?;
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
    check_cancel(tc)?;
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
impl ToolHandler for Read {
    fn name(&self) -> &str {
        "read"
    }
    fn definition(&self) -> Tool {
        schema(self.name(),"Read a UTF-8 text file with line numbers. offset is 1-based; use next_offset for subsequent pages. No images or binary files.",json!({"path":{"type":"string","minLength":1},"offset":{"type":"integer","minimum":1},"limit":{"type":"integer","minimum":1,"maximum":2000}}), &["path"])
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        security(self.name(), &self.cwd, input, false)
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: ReadInput = serde_json::from_value(input).map_err(|e| error(self.name(), e))?;
            if i.path.is_empty() || i.offset == 0 || !(1..=2000).contains(&i.limit) {
                return Err(error(self.name(), "invalid path, offset or limit"));
            }
            let path =
                resolve_path(&self.cwd, Path::new(&i.path)).map_err(|e| error(self.name(), e))?;
            let path_str = utf8_path(&path, self.name())?;
            file_permission(&tc, &path, false, self.name()).await?;
            let content = text(&path, self.name())?;
            let lines: Vec<_> = content.split_inclusive('\n').collect();
            if i.offset > lines.len() && !(lines.is_empty() && i.offset == 1) {
                return Err(error(self.name(), "offset is past end of file"));
            }
            let start = i.offset - 1;
            let end = start.saturating_add(i.limit).min(lines.len());
            let body = lines[start..end]
                .iter()
                .enumerate()
                .map(|(n, line)| format!("{}|{}", i.offset + n, line))
                .collect::<String>();
            check_cancel(&tc)?;
            Ok(
                json!({"ok":true,"path":path_str,"offset":i.offset,"next_offset":(end<lines.len()).then_some(end+1),"content":body}),
            )
        })
    }
}
impl ToolHandler for Write {
    fn name(&self) -> &str {
        "write"
    }
    fn definition(&self) -> Tool {
        schema(self.name(),"Create a UTF-8 file or OVERWRITE its entire content. Creates parent directories. Use edit for targeted changes.",json!({"path":{"type":"string","minLength":1},"content":{"type":"string"}}), &["path","content"])
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        security(self.name(), &self.cwd, input, true)
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: WriteInput = serde_json::from_value(input).map_err(|e| error(self.name(), e))?;
            let path = target(&self.cwd, &i.path, self.name())?;
            let path_str = utf8_path(&path, self.name())?;
            file_permission(&tc, &path, true, self.name()).await?;
            let lock = file_lock(&path);
            let _guard = tokio::select! {biased;_=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),g=lock.lock()=>g};
            let created = save(&path, &i.content, &tc, self.name())?;
            Ok(json!({"ok":true,"path":path_str,"created":created,"bytes_written":i.content.len()}))
        })
    }
}
impl ToolHandler for Edit {
    fn name(&self) -> &str {
        "edit"
    }
    fn definition(&self) -> Tool {
        schema(self.name(),"Replace one exact, unique occurrence of old_text in an existing UTF-8 file. Include enough context to make the match unique. No fuzzy matching; new_text may be empty to delete text. Returns a diff.",json!({"path":{"type":"string","minLength":1},"old_text":{"type":"string","minLength":1},"new_text":{"type":"string"}}), &["path","old_text","new_text"])
    }
    fn security_context(&self, input: &Value) -> SecurityContext {
        security(self.name(), &self.cwd, input, true)
    }
    fn execute<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: EditInput = serde_json::from_value(input).map_err(|e| error(self.name(), e))?;
            if i.old_text.is_empty() {
                return Err(error(self.name(), "old_text must be nonempty"));
            }
            let path = target(&self.cwd, &i.path, self.name())?;
            let path_str = utf8_path(&path, self.name())?;
            file_permission(&tc, &path, true, self.name()).await?;
            let lock = file_lock(&path);
            let _guard = tokio::select! {biased;_=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),g=lock.lock()=>g};
            let before = text(&path, self.name())?;
            let Some(start) = before.find(&i.old_text) else {
                return Err(error(
                    self.name(),
                    "old_text not found; read the file again",
                ));
            };
            let next = start + before[start..].chars().next().unwrap().len_utf8();
            if before[next..].contains(&i.old_text) {
                return Err(error(
                    self.name(),
                    "old_text matches more than once; include more context",
                ));
            }
            let new_len = before.len() - i.old_text.len() + i.new_text.len();
            if new_len > MAX_FILE_BYTES {
                return Err(error(self.name(), "edited file exceeds 16 MiB"));
            }
            let after = before.replacen(&i.old_text, &i.new_text, 1);
            let changed = before != after;
            let label = path_str;
            let diff = similar::TextDiff::configure()
                .timeout(std::time::Duration::from_millis(200))
                .diff_lines(&before, &after)
                .unified_diff()
                .header(label, label)
                .to_string();
            if changed {
                save(&path, &after, &tc, self.name())?;
            } else {
                check_cancel(&tc)?;
            }
            Ok(json!({"ok":true,"path":path_str,"changed":changed,"diff":diff}))
        })
    }
}
