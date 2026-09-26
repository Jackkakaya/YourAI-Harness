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
/// opencode `read-filesystem.ts` `MAX_LINE_SUFFIX`, verbatim.
const LINE_SUFFIX: &str = "... (line truncated to 2000 chars)";
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
    // Files older than the retention window (mtime before now - RETENTION).
    let cutoff = std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(RETENTION_SECS))
        .unwrap_or(std::time::UNIX_EPOCH);
    let entries = match fs::read_dir(&dir) {
        Ok(e) => e,
        Err(_) => return,
    };
    for entry in entries.flatten() {
        let stale = entry
            .metadata()
            .ok()
            .and_then(|m| m.modified().ok())
            .is_some_and(|modified| modified < cutoff);
        if stale {
            let _ = fs::remove_file(entry.path());
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
/// mirroring opencode's `ToolOutputStore.bound`. opencode keeps the head AND
/// tail halves of the budget (the tail usually carries exit codes and error
/// summaries) with the truncation marker in between; lines that survive are
/// first clipped to `MAX_LINE_LENGTH`. When truncation occurs and a spill
/// directory is configured, the full text is written to a file and the
/// marker points the model at it. Returns the kept content plus a removal
/// summary.
pub fn output(text: &str, call_id: &str) -> Truncated {
    let limits = config().lock().unwrap().limits;
    output_with(text, call_id, limits, true)
}

/// Byte-boundary-safe prefix of at most `max_bytes` bytes.
fn take_prefix_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    &text[..end]
}

/// Byte-boundary-safe suffix of at most `max_bytes` bytes.
fn take_suffix_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }
    let mut start = text.len() - max_bytes;
    while start < text.len() && !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

/// Same as [`output`] but with explicit limits and optional spill (used by
/// tests to avoid touching the global spill directory).
pub fn output_with(text: &str, call_id: &str, limits: Limits, write_spill: bool) -> Truncated {
    let total_bytes = text.len();
    let lines: Vec<&str> = text.split('\n').collect();

    // No truncation needed; still clip long single lines in place. Clipping
    // is self-describing (inline suffix) and does not count as truncation —
    // every line was still delivered.
    if lines.len() <= limits.max_lines && total_bytes <= limits.max_bytes {
        let mut content = String::with_capacity(text.len());
        for (i, line) in lines.iter().enumerate() {
            if i > 0 {
                content.push('\n');
            }
            if line.chars().count() > MAX_LINE_LENGTH {
                content.push_str(&clip_line(line));
            } else {
                content.push_str(line);
            }
        }
        return Truncated {
            content,
            removed: 0,
            unit: "lines",
            spill_path: None,
        };
    }

    let spill_path = write_spill
        .then(|| spill(text, call_id))
        .flatten();

    // opencode boundedPreview: reserve room for the marker, then sample the
    // head and tail halves of the line budget, then bound the bytes of each
    // half the same way.
    let line_budget = limits.max_lines.saturating_sub(4).max(1);
    // ~80 bytes covers the longest marker line; exactness is not required
    // for the budget, only for the reported removal count.
    let byte_budget = limits.max_bytes.saturating_sub(80).max(1);
    let head_b = byte_budget.div_ceil(2);
    let tail_b = byte_budget - head_b;
    let clip_join = |ls: &[&str]| -> String {
        ls.iter()
            .map(|l| {
                if l.chars().count() > MAX_LINE_LENGTH {
                    clip_line(l)
                } else {
                    (*l).to_owned()
                }
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    let (head, tail, removed, unit) = if lines.len() > line_budget {
        // Line sampling: head and tail halves of the line budget.
        let head_n = line_budget.div_ceil(2);
        let tail_n = line_budget - head_n;
        let head = clip_join(&lines[..head_n]);
        let tail = if tail_n > 0 {
            clip_join(&lines[lines.len() - tail_n..])
        } else {
            String::new()
        };
        let head = take_prefix_bytes(&head, head_b).to_owned();
        let tail = take_suffix_bytes(&tail, tail_b).to_owned();
        let removed = lines.len().saturating_sub(head_n + tail_n);
        (head, tail, removed, "lines")
    } else {
        let joined = lines.join("\n");
        if joined.len() <= byte_budget {
            // Few lines, within bytes after clipping: cannot happen (the
            // early return above already covered it) but keep it total.
            let removed = total_bytes.saturating_sub(joined.len());
            (joined, String::new(), removed, "bytes")
        } else {
            // Byte overflow with few lines: split the byte budget head/tail.
            let head = take_prefix_bytes(&joined, head_b).to_owned();
            let tail = take_suffix_bytes(&joined, tail_b).to_owned();
            let removed = total_bytes.saturating_sub(head.len() + tail.len());
            (head, tail, removed, "bytes")
        }
    };
    // opencode marker, verbatim format: points at the spill file when one
    // was written, otherwise reports the removed amount.
    let marker = match &spill_path {
        Some(path) => format!(
            "... output truncated; full content saved to {} ...",
            path.display()
        ),
        None => format!("...{removed} {unit} truncated..."),
    };

    let mut content = head;
    content.push_str("\n\n");
    content.push_str(&marker);
    if !tail.is_empty() {
        content.push_str("\n\n");
        content.push_str(&tail);
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
    fn too_many_lines_keeps_head_and_tail_and_counts_removed_lines() {
        let text: String = (0..2500)
            .map(|i| format!("line {i}"))
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c1", lm(), false);
        assert!(t.is_truncated());
        assert_eq!(t.unit, "lines");
        // line_budget = MAX_LINES - 4 (marker room); head+tail keep that many.
        assert_eq!(t.removed, 2500 - (MAX_LINES - 4));
        // opencode samples head AND tail: first and last lines survive, the
        // middle does not.
        assert!(t.content.contains("line 0"), "head kept: {}", &t.content[..40]);
        assert!(t.content.contains("line 2499"), "tail kept");
        assert!(!t.content.contains("\nline 1500\n"), "middle dropped");
        assert!(t.content.contains("lines truncated..."));
    }

    #[test]
    fn too_many_bytes_keeps_head_and_tail() {
        let line = "x".repeat(1000);
        let text: String = std::iter::repeat_n(line, 200)
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c2", lm(), false);
        assert!(t.is_truncated());
        assert_eq!(t.unit, "bytes");
        assert!(t.removed > 0);
        // Both halves of the byte budget are used.
        assert!(t.content.starts_with("xxxx"), "head bytes kept");
        assert!(t.content.ends_with("xxxx"), "tail bytes kept");
        assert!(t.content.contains("bytes truncated..."));
    }

    #[test]
    fn long_single_line_is_clipped_without_counting_as_truncation() {
        let t = output_with(&"a".repeat(5000), "c3", lm(), false);
        assert!(t.content.contains(LINE_SUFFIX));
        assert!(!t.is_truncated(), "clipping one line keeps every line");
        assert!(footnote(&t).is_none());
    }

    #[test]
    fn line_overflow_reports_lines_even_when_bytes_also_exceed() {
        let text: String = (0..3000)
            .map(|_| "012345678901234567890123456789")
            .collect::<Vec<_>>()
            .join("\n");
        let t = output_with(&text, "c4", lm(), false);
        assert!(t.is_truncated());
        // Line count is the primary trigger (3000 > 2000).
        assert_eq!(t.unit, "lines");
        // The byte budget still bounds the sampled halves.
        assert!(t.content.len() < 60 * 1024);
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
        // Tiny budgets keep only the marker room: line_budget = 5-4 = 1.
        assert_eq!(t.unit, "lines");
        assert!(t.content.contains("line 0"));
        assert_eq!(t.removed, 9);
    }
}
