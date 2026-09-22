//! `@` file-mention autocomplete.
//!
//! When the user types `@` (preceded by whitespace or at line start) the
//! editor enters mention mode: files under the current working directory are
//! listed, filtered by the text after `@`, and selected with Up/Down + Tab/Enter.
//!
//! On selection the file is classified:
//! - **Text** (code, markdown, config, …) → file contents are inlined into
//!   the message text as a fenced block.
//! - **Image / PDF** → added as a [`UserAttachment`](yourai_core::protocol::UserAttachment)
//!   binary part, reusing the same pipeline as clipboard paste.
//!
//! This is the SSH-safe path: it reads the local filesystem, not the clipboard,
//! so it works inside OrbStack containers and remote shells.

use std::path::{Path, PathBuf};

/// A single file entry in the mention list.
#[derive(Clone)]
pub struct MentionEntry {
    /// Path relative to cwd, using `/` separators.
    pub display: String,
    /// Absolute path on disk.
    pub path: PathBuf,
    /// True for directories (shown but not attachable).
    pub is_dir: bool,
}

/// State for the `@` autocomplete popup.
#[derive(Default)]
pub struct MentionState {
    /// Entries matching the current query (after `@`).
    pub entries: Vec<MentionEntry>,
    pub selected: usize,
    /// Byte offset of the `@` in the editor text.
    pub anchor: usize,
    /// The query text (everything after `@`, no spaces).
    pub query: String,
    pub active: bool,
}

impl MentionState {
    /// Check whether `text` (editor content) with `cursor` position triggers
    /// or updates mention mode. Returns the anchor + query when active.
    pub fn detect(text: &str, cursor: usize) -> Option<(usize, String)> {
        // Look backwards from cursor for `@`.
        let before = &text[..cursor];
        let at_pos = before.rfind('@')?;
        // `@` must be at line start or preceded by whitespace.
        let prev = if at_pos == 0 {
            ' '
        } else {
            before[..at_pos].chars().next_back().unwrap_or(' ')
        };
        if !prev.is_whitespace() {
            return None;
        }
        let query = &before[at_pos + 1..];
        // Query must not contain whitespace (end of mention).
        if query.chars().any(|c| c.is_whitespace()) {
            return None;
        }
        Some((at_pos, query.to_owned()))
    }

    pub fn activate(&mut self, anchor: usize, query: &str) {
        self.anchor = anchor;
        self.query = query.to_owned();
        self.active = true;
        self.selected = 0;
    }

    pub fn deactivate(&mut self) {
        self.active = false;
        self.entries.clear();
    }

    pub fn step(&mut self, backwards: bool) {
        let count = self.entries.len();
        if count > 0 {
            self.selected = if backwards {
                (self.selected + count - 1) % count
            } else {
                (self.selected + 1) % count
            };
        }
    }

    pub fn current(&self) -> Option<&MentionEntry> {
        self.entries.get(self.selected)
    }
}

/// Directories to always skip during file scanning.
const SKIP_DIRS: &[&str] = &[
    ".git",
    "node_modules",
    ".svn",
    ".hg",
    "target",
    "dist",
    "build",
    ".next",
    ".cache",
    "__pycache__",
    ".venv",
    "venv",
    ".tox",
    ".mypy_cache",
    ".pytest_cache",
    "coverage",
    ".idea",
    ".vscode",
    ".yourai",
];

/// Maximum number of entries to list.
const MAX_ENTRIES: usize = 50;

/// Maximum number of directory entries inspected by one query. This bounds
/// worst-case work when the query has no matches.
const MAX_SCANNED_ENTRIES: usize = 2_000;

/// Maximum scan depth (relative to cwd).
const MAX_DEPTH: usize = 4;

/// Scan the working directory for files matching `query`.
///
/// The scan is recursive (up to [`MAX_DEPTH`]) but skips common
/// vendor/build directories. Results are sorted: directories first, then
/// files, each group alphabetical. The list is capped at [`MAX_ENTRIES`].
pub fn scan(cwd: &Path, query: &str) -> Vec<MentionEntry> {
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    let mut scanned = 0;
    scan_dir(
        cwd,
        cwd,
        query,
        0,
        &mut scanned,
        &mut dirs,
        &mut files,
    );
    dirs.sort_by(|a, b| a.display.cmp(&b.display));
    files.sort_by(|a, b| a.display.cmp(&b.display));
    dirs.extend(files);
    dirs.truncate(MAX_ENTRIES);
    dirs
}

fn scan_dir(
    cwd: &Path,
    dir: &Path,
    query: &str,
    depth: usize,
    scanned: &mut usize,
    dirs: &mut Vec<MentionEntry>,
    files: &mut Vec<MentionEntry>,
) {
    if depth > MAX_DEPTH
        || *scanned >= MAX_SCANNED_ENTRIES
        || dirs.len() + files.len() >= MAX_ENTRIES
    {
        return;
    }
    let Ok(read_dir) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in read_dir.flatten() {
        if *scanned >= MAX_SCANNED_ENTRIES || dirs.len() + files.len() >= MAX_ENTRIES {
            return;
        }
        *scanned += 1;
        let path = entry.path();
        let file_name = entry.file_name();
        let Some(name) = file_name.to_str() else {
            continue;
        };
        let name = name.to_owned();
        if name.starts_with('.') && depth == 0 && SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        // Skip hidden files/dirs unless the query starts with '.'
        if name.starts_with('.') && !query.starts_with('.') {
            continue;
        }
        if SKIP_DIRS.contains(&name.as_str()) {
            continue;
        }
        let is_dir = entry
            .file_type()
            .map(|t| t.is_dir())
            .unwrap_or(false);
        let display = path
            .strip_prefix(cwd)
            .ok()
            .and_then(|p| p.to_str())
            .unwrap_or(&name)
            .replace('\\', "/");
        // Filter: the display path must contain the query (case-insensitive).
        if !query.is_empty() && !display.to_lowercase().contains(&query.to_lowercase()) {
            // Even if the directory itself does not match, a child might.
            if is_dir && depth < MAX_DEPTH {
                scan_dir(cwd, &path, query, depth + 1, scanned, dirs, files);
            }
            continue;
        }
        if is_dir {
            dirs.push(MentionEntry {
                display,
                path: path.clone(),
                is_dir: true,
            });
            // Recurse into matching directories too.
            scan_dir(cwd, &path, query, depth + 1, scanned, dirs, files);
        } else {
            files.push(MentionEntry {
                display,
                path,
                is_dir: false,
            });
        }
    }
}

/// Read the first-level entries of a directory and format them as structured
/// text, aligned with opencode's Read tool output for directories:
///
/// ```text
/// <path>/abs/path/to/dir</path>
/// <type>directory</type>
/// <entries>
/// main.rs
/// utils/
/// mod.rs
/// (3 entries)
/// </entries>
/// ```
///
/// Directories get a trailing `/`. Entries are sorted alphabetically
/// (directories first, then files). Returns `None` if the directory cannot
/// be read.
pub fn read_directory_listing(path: &Path) -> Option<String> {
    let entries = std::fs::read_dir(path).ok()?;
    let mut dirs = Vec::new();
    let mut files = Vec::new();
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir {
            dirs.push(format!("{name}/"));
        } else {
            files.push(name);
        }
    }
    dirs.sort();
    files.sort();
    let total = dirs.len() + files.len();
    let mut lines = Vec::with_capacity(total + 5);
    lines.push(format!("<path>{}</path>", path.display()));
    lines.push("<type>directory</type>".to_owned());
    lines.push("<entries>".to_owned());
    lines.extend(dirs);
    lines.extend(files);
    lines.push(format!("({total} entries)"));
    lines.push("</entries>".to_owned());
    Some(lines.join("\n"))
}

/// Classify a file by its extension into a content category.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// Readable text: code, markdown, config, etc.
    Text,
    /// Image file (png, jpg, gif, webp, …).
    Image,
    /// PDF document.
    Pdf,
    /// Binary, not directly attachable as media.
    Other,
}

/// Map a file path to a kind. Aligned with the MIME table in the harness
/// `attachment_to_part` validator (image/*, audio/*, application/pdf).
pub fn classify(path: &Path) -> FileKind {
    let ext = path
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.to_ascii_lowercase())
        .unwrap_or_default();
    match ext.as_str() {
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "avif" | "bmp" | "tiff" | "tif" => FileKind::Image,
        "pdf" => FileKind::Pdf,
        // Common text/code extensions.
        "rs" | "go" | "py" | "js" | "ts" | "tsx" | "jsx" | "java" | "c" | "cpp" | "h" | "hpp"
        | "cs" | "rb" | "php" | "swift" | "kt" | "scala" | "clj" | "ex" | "exs" | "erl"
        | "lua" | "sh" | "bash" | "zsh" | "fish" | "ps1" | "bat" | "cmd" | "sql" | "graphql"
        | "proto" | "thrift" | "toml" | "yaml" | "yml" | "json" | "xml" | "html" | "css"
        | "scss" | "sass" | "less" | "md" | "markdown" | "rst" | "txt" | "log" | "ini"
        | "cfg" | "conf" | "env" | "gitignore" | "dockerignore" | "dockerfile" | "makefile"
        | "cmake" | "gradle" | "csv" | "tsv" | "lock" | "diff" | "patch" | "vim" | "el"
        | "lisp" | "hs" | "ml" | "nim" | "zig" | "v" | "dart" | "r" | "jl" | "pl" | "asm"
        | "s" | "wasm" | "wat" => FileKind::Text,
        _ => {
            // No extension or unknown: try reading a prefix to detect text.
            if ext.is_empty() {
                FileKind::Text // assume text for extensionless files (README, Makefile, …)
            } else {
                FileKind::Other
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_at_at_line_start() {
        assert_eq!(
            MentionState::detect("@foo", 4),
            Some((0, "foo".to_owned()))
        );
    }

    #[test]
    fn detect_at_after_space() {
        assert_eq!(
            MentionState::detect("hello @fo", 9),
            Some((6, "fo".to_owned()))
        );
    }

    #[test]
    fn no_detect_after_non_space() {
        assert_eq!(MentionState::detect("hello@foo", 9), None);
    }

    #[test]
    fn no_detect_with_space_in_query() {
        assert_eq!(MentionState::detect("@foo bar", 8), None);
    }

    #[test]
    fn detect_empty_query_right_after_at() {
        assert_eq!(MentionState::detect("hello @", 7), Some((6, "".to_owned())));
    }

    #[test]
    fn classify_known_extensions() {
        assert_eq!(classify(Path::new("foo.png")), FileKind::Image);
        assert_eq!(classify(Path::new("foo.JPG")), FileKind::Image);
        assert_eq!(classify(Path::new("doc.pdf")), FileKind::Pdf);
        assert_eq!(classify(Path::new("main.rs")), FileKind::Text);
        assert_eq!(classify(Path::new("README.md")), FileKind::Text);
        assert_eq!(classify(Path::new("config.yaml")), FileKind::Text);
        assert_eq!(classify(Path::new("archive.zip")), FileKind::Other);
    }

    #[test]
    fn classify_extensionless_as_text() {
        assert_eq!(classify(Path::new("Makefile")), FileKind::Text);
        assert_eq!(classify(Path::new("README")), FileKind::Text);
    }

    #[test]
    fn scan_finds_files_and_filters_by_query() {
        let dir = std::env::temp_dir().join("yourai_mention_test");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.rs"), "fn main() {}").unwrap();
        std::fs::write(dir.join("beta.py"), "print(1)").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

        let results = scan(&dir, "");
        let names: Vec<&str> = results.iter().map(|e| e.display.as_str()).collect();
        assert!(names.contains(&"alpha.rs"));
        assert!(names.contains(&"beta.py"));
        assert!(names.contains(&"src"));

        let filtered = scan(&dir, "alpha");
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].display, "alpha.rs");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scan_skips_vendor_dirs() {
        let dir = std::env::temp_dir().join("yourai_mention_skip");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(dir.join("node_modules")).unwrap();
        std::fs::write(dir.join("node_modules/hidden.js"), "").unwrap();
        std::fs::write(dir.join("visible.rs"), "").unwrap();

        let results = scan(&dir, "");
        let names: Vec<&str> = results.iter().map(|e| e.display.as_str()).collect();
        assert!(names.contains(&"visible.rs"));
        assert!(!names.iter().any(|n| n.contains("node_modules")));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn mention_step_wraps_around() {
        let mut m = MentionState {
            entries: vec![
                MentionEntry {
                    display: "a".into(),
                    path: PathBuf::from("a"),
                    is_dir: false,
                },
                MentionEntry {
                    display: "b".into(),
                    path: PathBuf::from("b"),
                    is_dir: false,
                },
            ],
            ..Default::default()
        };
        assert_eq!(m.selected, 0);
        m.step(false);
        assert_eq!(m.selected, 1);
        m.step(false);
        assert_eq!(m.selected, 0);
        m.step(true);
        assert_eq!(m.selected, 1);
    }

    #[test]
    fn read_directory_listing_formats_entries() {
        let dir = std::env::temp_dir().join("yourai_mention_listing");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.rs"), "").unwrap();
        std::fs::write(dir.join("beta.py"), "").unwrap();
        std::fs::create_dir_all(dir.join("src")).unwrap();

        let listing = read_directory_listing(&dir).unwrap();
        assert!(listing.contains("<type>directory</type>"));
        assert!(listing.contains("<entries>"));
        assert!(listing.contains("src/"));
        assert!(listing.contains("alpha.rs"));
        assert!(listing.contains("beta.py"));
        assert!(listing.contains("(3 entries)"));
        // Directories must come before files.
        let src_pos = listing.find("src/").unwrap();
        let alpha_pos = listing.find("alpha.rs").unwrap();
        assert!(src_pos < alpha_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn read_directory_listing_none_for_unreadable() {
        let result = read_directory_listing(Path::new("/nonexistent_yourai_test_dir_12345"));
        assert!(result.is_none());
    }
}
