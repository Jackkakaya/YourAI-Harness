//! Centralized tool-output truncation, aligned with opencode's `Truncate`.
//!
//! opencode routes every tool result through a shared `Truncate.output` service
//! with `MAX_LINES = 2000` and `MAX_BYTES = 50 * 1024`. It also clips long
//! single lines to `MAX_LINE_LENGTH = 2000` chars (see `tool/read.ts`). When a
//! result exceeds the budget, opencode writes the full text to a spill file
//! under `Global.Path.data/tool-output` (7-day retention, hourly cleanup) and
//! returns a preview plus a hint pointing at the saved file.
//!
//! YourAI mirrors this: [`output`] truncates to the model-facing budget and,
//! when configured with a spill directory via [`init`], writes the full text to
//! a file named after the tool call so the model can re-read it with the `read`
//! tool. Limits are configurable via [`set_limits`]; compile-time constants are
//! the defaults.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

/// opencode `Truncate.MAX_LINES`: maximum lines retained in a tool result.
pub const MAX_LINES: usize = 2000;
/// opencode `Truncate.MAX_BYTES`: maximum bytes retained in a tool result.
pub const MAX_BYTES: usize = 50 * 1024;
/// opencode `read.ts` `MAX_LINE_LENGTH`: single lines longer than this are
/// clipped with a suffix.
pub const MAX_LINE_LENGTH: usize = 2000;
const LINE_SUFFIX: &str = "... (line truncated)";
/// opencode `Truncate.RETENTION`: spill files older than this are removed.
const RETENTION_SECS: u64 = 7 * 24 * 60 * 60;

/// Resolved truncation limits. Defaults match opencode's compile-time constants.
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    pub max_lines: usize,
    pub max_bytes: usize,
}
impl Default for Limits {
    fn default() -> Self {
        Self {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
        }
    }
}

#[derive(Default)]
struct Config {
    limits: Limits,
    spill_dir: Option<PathBuf>,
}
static CONFIG: OnceLock<std::sync::Mutex<Config>> = OnceLock::new();
fn config() -> &'static std::sync::Mutex<Config> {
    CONFIG.get_or_init(|| std::sync::Mutex::new(Config::default()))
}

/// Initialize the spill directory and limits. Called once during harness
/// assembly with the session data root; opencode uses
/// `Global.Path.data/tool-output`. Safe to call before any tool runs; a missing
/// call leaves truncation working without spill files (tail is dropped).
pub fn init(spill_dir: PathBuf, limits: Limits) {
    let mut c = config().lock().unwrap();
    c.limits = limits;
    c.spill_dir = Some(spill_dir);
}

/// Override just the limits (e.g. from config without a spill directory).
pub fn set_limits(limits: Limits) {
    config().lock().unwrap().limits = limits;
}

/// Remove spill files older than [`RETENTION_SECS`]. opencode runs this hourly;
/// YourAI exposes it for the host to schedule. Errors are best-effort ignored.
pub fn cleanup() {
    let dir = match config().lock().unwrap().spill_dir.clone() {
        Some(d) => d,
        None => return,
    };
    let cutoff = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs().saturating_sub(RETENTION_SECS))
        .unwrap_or(0);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "txt") {
            continue;
        }
        let age = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|m| m.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|d| d.as_secs())
            .unwrap_or(0);
        if age < cutoff {
            let _ = fs::remove_file(&path);
        }
    }
}

/// Write the full text to a spill file and return its path. Returns None when
/// no spill directory is configured or the write fails.
fn spill(text: &str, call_id: &str) -> Option<PathBuf> {
    let dir = config().lock().unwrap().spill_dir.clone()?;
    fs::create_dir_all(&dir).ok()?;
    // opencode names spill files with an ascending tool identifier; YourAI uses
    // the tool call id for traceability and to avoid collisions.
    let safe = call_id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect::<String>();
    let file = dir.join(format!("{safe}.txt"));
    fs::write(&file, text).ok()?;
    Some(file)
}

/// A truncation outcome. `removed` counts what was dropped (lines or bytes).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Truncated {
    pub content: String,
    pub removed: usize,
    pub unit: &'static str,
    /// Spill file path when the full output was saved, mirroring opencode.
    pub spill_path: Option<PathBuf>,
}
impl Truncated {
    /// True when some content was removed.
    pub fn is_truncated(&self) -> bool {
        self.removed > 0
    }
}

/// Clip a single line to `MAX_LINE_LENGTH` characters and append the suffix
/// when truncated (mirrors opencode `read.ts` `MAX_LINE_SUFFIX`).
pub fn clip_line(line: &str) -> String {
    if line.chars().count() <= MAX_LINE_LENGTH {
        return line.to_string();
    }
    let end = line
        .char_indices()
        .nth(MAX_LINE_LENGTH)
        .map(|(i, _)| i)
        .unwrap_or(line.len());
    format!("{}{}", &line[..end], LINE_SUFFIX)
}

/// Truncate text to fit within the configured `max_lines` and `max_bytes`,
/// mirroring opencode's head-direction `Truncate.output`. Each retained line is
/// first clipped to `MAX_LINE_LENGTH`. When truncation occurs and a spill
/// directory is configured, the full text is written to a file and a hint is
/// appended to the content. Returns the kept content plus a removal summary.
pub fn output(text: &str, call_id: &str) -> Truncated {
    let limits = config().lock().unwrap().limits;
    output_with(text, call_id, limits, true)
}

/// Same as [`output`] but with explicit limits and optional spill (used by
/// tests to avoid touching the global spill directory).
pub fn output_with(text: &str, call_id: &str, limits: Limits, write_spill: bool) -> Truncated {
    let total_bytes = text.len();
    let lines: Vec<&str> = text.split('\n').collect();

    // No truncation needed; still clip long single lines in place.
    if lines.len() <= limits.max_lines && total_bytes <= limits.max_bytes {
        let mut content = String::with_capacity(text.len());
        let mut clipped_any = false;
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.push('\n');
            }
            if line.chars().count() > MAX_LINE_LENGTH {
                content.push_str(&clip_line(line));
                clipped_any = true;
            } else {
                content.push_str(line);
            }
        }
        return Truncated {
            content,
            removed: if clipped_any { 1 } else { 0 },
            unit: "lines",
            spill_path: None,
        };
    }

    let mut out: Vec<String> = Vec::new();
    let mut bytes = 0usize;
    let mut hit_bytes = false;
    for line in lines.iter().take(limits.max_lines) {
        let clipped = if line.chars().count() > MAX_LINE_LENGTH {
            clip_line(line)
        } else {
            (*line).to_string()
        };
        let size = clipped.len() + if out.is_empty() { 0 } else { 1 };
        if bytes + size > limits.max_bytes {
            hit_bytes = true;
            break;
        }
        out.push(clipped);
        bytes += size;
    }
    let (removed, unit) = if hit_bytes {
        (total_bytes.saturating_sub(bytes), "bytes")
    } else {
        (lines.len().saturating_sub(out.len()), "lines")
    };
    let spill_path = if write_spill {
        spill(text, call_id)
    } else {
        None
    };
    let mut content = out.join("\n");
    content.push_str(&format!("\n\n...{removed} {unit} truncated..."));
    if let Some(path) = &spill_path {
        // opencode hint: point the model at the saved file for Read/Grep.
        content.push_str(&format!(
            "\n\nFull output saved to: {}\nUse the read tool with offset/limit to view sections, or shell `grep` to search.",
            path.display()
        ));
    }
    Truncated {
        content,
        removed,
        unit,
        spill_path,
    }
}

/// Format a truncation footnote (`...N {unit} truncated...`) for callers that
/// report truncation as a separate field rather than inline in content.
pub fn footnote(t: &Truncated) -> Option<String> {
    if t.is_truncated() {
        Some(format!("...{} {} truncated...", t.removed, t.unit))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lm() -> Limits {
        Limits {
            max_lines: MAX_LINES,
            max_bytes: MAX_BYTES,
        }
    }

    #[test]
    fn short_text_is_unchanged() {
        let t = output_with("hello\nworld", "c0", lm(), false);
        assert_eq!(t.content, "hello\nworld");
        assert!(!t.is_truncated());
        assert!(footnote(&t).is_none());
        assert!(t.spill_path.is_none());
    }

    #[test]
    fn too_many_lines_keeps_head_and_counts_removed_lines() {
        let text: String = (0..2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c1", lm(), false);
        assert!(t.is_truncated());
        assert_eq!(t.unit, "lines");
        assert_eq!(t.removed, 500);
        // The preview is the first MAX_LINES lines; the footnote follows.
        let preview = t.content.split("\n\n...").next().unwrap_or(&t.content);
        let kept: Vec<&str> = preview.split('\n').collect();
        assert_eq!(kept.len(), MAX_LINES);
        assert_eq!(kept[0], "line 0");
        assert!(t.content.contains("...500 lines truncated..."));
    }

    #[test]
    fn too_many_bytes_stops_at_byte_budget() {
        let line = "x".repeat(1000);
        let text: String = std::iter::repeat_n(line, 200)
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c2", lm(), false);
        assert!(t.is_truncated());
        assert_eq!(t.unit, "bytes");
        assert!(t.removed > 0);
    }

    #[test]
    fn long_single_line_is_clipped_within_line_limit() {
        let t = output_with(&"a".repeat(5000), "c3", lm(), false);
        assert!(t.content.contains(LINE_SUFFIX));
    }

    #[test]
    fn byte_truncation_takes_precedence_over_line_clipping() {
        let text: String = (0..3000)
            .map(|_| "012345678901234567890123456789")
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c4", lm(), false);
        assert!(t.is_truncated());
        assert_eq!(t.unit, "bytes");
    }

    #[test]
    fn clip_line_preserves_short_lines() {
        assert_eq!(clip_line("short"), "short");
    }

    #[test]
    fn clip_line_truncates_long_lines_with_suffix() {
        let long = "b".repeat(3000);
        let clipped = clip_line(&long);
        assert!(clipped.ends_with(LINE_SUFFIX));
        assert!(clipped.chars().count() <= MAX_LINE_LENGTH + LINE_SUFFIX.chars().count());
    }

    #[test]
    fn spill_file_is_written_when_dir_configured() {
        let dir = tempfile::tempdir().unwrap();
        init(dir.path().to_path_buf(), lm());
        let text: String = (0..3000)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = output(&text, "call-xyz");
        assert!(t.is_truncated());
        let path = t.spill_path.expect("spill file written");
        assert!(path.exists());
        assert_eq!(fs::read_to_string(&path).unwrap(), text);
        assert!(t.content.contains(&path.display().to_string()));
        // Reset so other tests don't get a spill dir.
        config().lock().unwrap().spill_dir = None;
    }

    #[test]
    fn custom_limits_override_defaults() {
        let limits = Limits {
            max_lines: 5,
            max_bytes: MAX_BYTES,
        };
        let text: String = (0..10)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c5", limits, false);
        assert!(t.is_truncated());
        assert_eq!(t.removed, 5);
        assert_eq!(t.unit, "lines");
    }
}
