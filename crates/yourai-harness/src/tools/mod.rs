//! Built-in tools implement Core's existing ToolHandler; no loop or storage dependency.
mod files;
mod shell;
mod truncate;
mod web;
pub use web::{WebFetch, WebSearch};

pub use files::{Edit, Read, Write};
mod registry;
pub mod result;
pub use registry::ToolSet;
pub use truncate::{
    cleanup as truncate_cleanup, clip_line, footnote, init as init_truncation,
    output as truncate_output, set_limits as set_truncate_limits, Limits as TruncateLimits,
    MAX_BYTES, MAX_LINES, MAX_LINE_LENGTH,
};

use serde_json::{json, Value};
pub use shell::Shell;
use std::{
    path::{Component, Path, PathBuf},
    sync::Arc,
};
use yourai_core::prelude::*;

pub const MAX_FILE_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_OUTPUT_BYTES: usize = 8 * 1024 * 1024;
pub const SHELL_TIMEOUT_MS: u64 = 120_000;
pub const MAX_SHELL_TIMEOUT_MS: u64 = 600_000;

pub fn coding_tools(cwd: &Path) -> Result<Vec<Arc<dyn ToolHandler>>, YourAiError> {
    let cwd = std::fs::canonicalize(cwd).map_err(|e| error("tools", e))?;
    if !cwd.is_dir() {
        return Err(error("tools", "cwd must be a directory"));
    }
    Ok(vec![
        Arc::new(Read::new(cwd.clone())),
        Arc::new(Write::new(cwd.clone())),
        Arc::new(Edit::new(cwd.clone())),
        Arc::new(Shell::new(cwd)),
        Arc::new(WebFetch::new()?),
        Arc::new(WebSearch::new()?),
    ])
}

pub(crate) fn error(tool: &str, message: impl std::fmt::Display) -> YourAiError {
    ErrorKind::Tool {
        name: tool.into(),
        message: message.to_string(),
    }
    .into()
}
pub(crate) fn check_cancel(tc: &ToolContext<'_>) -> Result<(), YourAiError> {
    if tc.cancel.is_cancelled() {
        Err(AbortReason::Cancelled.into())
    } else {
        Ok(())
    }
}
/// serde_json cannot serialize non-UTF-8 paths (`json!` unwraps internally and
/// would panic). Tool result JSON must only embed checked display strings.
pub(crate) fn utf8_path<'a>(path: &'a Path, name: &str) -> Result<&'a str, YourAiError> {
    path.to_str()
        .ok_or_else(|| error(name, "path is not UTF-8"))
}
pub(crate) fn schema(name: &str, description: &str, properties: Value, required: &[&str]) -> Tool {
    Tool::new(name).with_description(description).with_schema(json!({
        "type":"object", "properties":properties,"required":required,"additionalProperties":false
    }))
}

/// Resolve each existing component (including symlinks) before processing `..`.
/// Missing final components are permitted for new files. Shared by approval and execution.
pub fn resolve_path(cwd: &Path, path: &Path) -> std::io::Result<PathBuf> {
    if !cwd.is_absolute() {
        return Err(std::io::Error::other("cwd must be absolute"));
    }
    let mut out = if path.is_absolute() {
        PathBuf::new()
    } else {
        std::fs::canonicalize(cwd)?
    };
    for part in path.components() {
        match part {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            Component::Normal(name) => {
                out.push(name);
                match std::fs::symlink_metadata(&out) {
                    Ok(_) => out = std::fs::canonicalize(&out)?,
                    Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                    Err(e) => return Err(e),
                }
            }
            Component::RootDir | Component::Prefix(_) => out.push(part.as_os_str()),
        }
    }
    Ok(out)
}

pub(crate) fn security(name: &str, cwd: &Path, input: &Value, write: bool) -> SecurityContext {
    let mut normalized = input.clone();
    let field = if name == "shell" { "cwd" } else { "path" };
    if let Some(obj) = normalized.as_object_mut() {
        let raw = input.get(field).and_then(Value::as_str).unwrap_or(".");
        match resolve_path(cwd, Path::new(raw)) {
            Ok(path) => {
                // Approval context is descriptive only; lossy keeps non-UTF-8
                // paths inspectable without panicking in json!. The executed
                // path itself is guarded by utf8_path/file_permission.
                obj.insert(field.into(), json!(path.to_string_lossy()));
            }
            Err(_) => {
                obj.insert("path_resolution_failed".into(), json!(true));
            }
        }
    }
    SecurityContext {
        action: name.into(),
        input: normalized,
        is_destructive: write,
        is_network: name == "shell",
    }
}

pub(crate) async fn file_permission(
    tc: &ToolContext<'_>,
    path: &Path,
    write: bool,
    name: &str,
) -> Result<(), YourAiError> {
    check_cancel(tc)?;
    if let Some(policy) = &tc.security {
        let path = path
            .to_str()
            .ok_or_else(|| error(name, "path is not UTF-8"))?;
        if !matches!(
            policy.check_file_access(path, write).await?,
            PolicyDecision::Allow
        ) {
            return Err(error(name, "file access denied by hard policy"));
        }
    }
    check_cancel(tc)
}
