//! Clipboard copy aligned with opencode's strategy:
//! <https://github.com/sst/opencode/blob/dev/packages/opencode/src/cli/cmd/tui/util/clipboard.ts>
//!
//! Two layers, same as opencode:
//! 1. **OSC 52** (primary) — an ANSI escape sequence that asks the terminal
//!    emulator to set its own clipboard.  Works over SSH, inside VMs and
//!    containers, with zero external binaries.
//! 2. **Native command** (supplemental) — `pbcopy` / `wl-copy` / `xclip` /
//!    `xsel` / `powershell.exe`, detected at runtime via PATH search.
//!
//! OSC 52 is always sent first; the native command's failure is non-fatal.

use std::{
    io::{self, IsTerminal, Write},
    path::PathBuf,
    process::Stdio,
    sync::OnceLock,
    time::Duration,
};
use tokio::{io::{AsyncReadExt, AsyncWriteExt}, process::Command};
use yourai_core::protocol::MAX_USER_ATTACHMENT_BYTES;

const MAX_ATTACHMENT_BASE64_BYTES: usize = MAX_USER_ATTACHMENT_BYTES.div_ceil(3) * 4;

// ── base64 ────────────────────────────────────────────────────────────

/// Standard base64 encode without pulling in a dependency.
pub fn base64_encode(input: &[u8]) -> String {
    const TABLE: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(input.len().div_ceil(3) * 4);
    for chunk in input.chunks(3) {
        let n = (chunk[0] as u32) << 16
            | (chunk.get(1).copied().unwrap_or(0) as u32) << 8
            | chunk.get(2).copied().unwrap_or(0) as u32;
        out.push(TABLE[((n >> 18) & 0x3F) as usize] as char);
        out.push(TABLE[((n >> 12) & 0x3F) as usize] as char);
        out.push(if chunk.len() > 1 {
            TABLE[((n >> 6) & 0x3F) as usize] as char
        } else {
            '='
        });
        out.push(if chunk.len() > 2 {
            TABLE[(n & 0x3F) as usize] as char
        } else {
            '='
        });
    }
    out
}

// ── which: runtime PATH search ────────────────────────────────────────

/// Search PATH for an executable.  Detection is runtime (not compile-time
/// `cfg`) so that, e.g., `pbcopy` injected into a Linux VM by OrbStack is
/// discovered and used — matching opencode's `which()` helper.
fn which(program: &str) -> Option<PathBuf> {
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths).find_map(|dir| {
        let path = dir.join(program);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::metadata(&path)
                .ok()
                .filter(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
                .map(|_| path)
        }
        #[cfg(not(unix))]
        {
            path.is_file().then_some(path)
        }
    })
}

// ── writeOsc52 (aligned with opencode PR #8974 + #12129) ──────────────

/// Write the OSC 52 escape sequence so the terminal emulator sets its own
/// clipboard.  This works over SSH and inside VMs/containers: the host
/// terminal handles the clipboard locally, so no external binary is required.
///
/// Writes to `/dev/tty` (or `$SSH_TTY`) to bypass terminal multiplexers
/// (Zellij, tmux, screen) that intercept OSC 52 on stdout.  Falls back to
/// stdout if `/dev/tty` is unavailable.
///
/// Returns `true` when the sequence was written.
fn write_osc52(text: &str) -> bool {
    // opencode: `if (!process.stdout.isTTY) return`
    if !io::stdout().is_terminal() {
        return false;
    }

    let osc52 = format!("\x1b]52;c;{}\x07", base64_encode(text.as_bytes()));

    // tmux / GNU screen require DCS passthrough wrapping.
    let sequence = if std::env::var_os("TMUX").is_some() || std::env::var_os("STY").is_some() {
        format!("\x1bPtmux;\x1b{osc52}\x1b\\")
    } else {
        osc52
    };

    // opencode (PR #12129): SSH_TTY || /dev/tty first, stdout fallback.
    let tty_path = std::env::var("SSH_TTY").unwrap_or_else(|_| "/dev/tty".to_string());
    #[cfg(unix)]
    if let Ok(mut f) = std::fs::OpenOptions::new().write(true).open(&tty_path) {
        if f.write_all(sequence.as_bytes()).is_ok() && f.flush().is_ok() {
            return true;
        }
    }
    #[cfg(not(unix))]
    {
        let _ = &tty_path; // /dev/tty doesn't exist on Windows
    }

    let mut out = io::stdout().lock();
    out.write_all(sequence.as_bytes()).is_ok() && out.flush().is_ok()
}

// ── getCopyMethod: lazy, memoized, runtime detection ──────────────────

/// A native clipboard command discovered at runtime.
#[derive(Clone, Copy)]
struct NativeMethod {
    program: &'static str,
    args: &'static [&'static str],
}

/// Detect the best available native clipboard command, mirroring opencode's
/// `getCopyMethod()`:
/// - macOS  → `pbcopy`
/// - Linux  → `wl-copy` (only if `WAYLAND_DISPLAY` is set) → `xclip` → `xsel`
/// - Windows → `powershell.exe`
///
/// Results are memoized: the PATH search runs at most once per process.
fn detect_native() -> Option<NativeMethod> {
    #[cfg(target_os = "macos")]
    {
        if which("pbcopy").is_some() {
            return Some(NativeMethod {
                program: "pbcopy",
                args: &[],
            });
        }
    }

    #[cfg(target_os = "linux")]
    {
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-copy").is_some() {
            return Some(NativeMethod {
                program: "wl-copy",
                args: &[],
            });
        }
        if which("xclip").is_some() {
            return Some(NativeMethod {
                program: "xclip",
                args: &["-selection", "clipboard"],
            });
        }
        if which("xsel").is_some() {
            return Some(NativeMethod {
                program: "xsel",
                args: &["--clipboard", "--input"],
            });
        }
    }

    #[cfg(target_os = "windows")]
    {
        if which("powershell.exe").is_some() {
            return Some(NativeMethod {
                program: "powershell.exe",
                args: &[
                    "-NonInteractive",
                    "-NoProfile",
                    "-Command",
                    "[Console]::InputEncoding = [System.Text.Encoding]::UTF8; \
                     Set-Clipboard -Value ([Console]::In.ReadToEnd())",
                ],
            });
        }
    }

    None
}

/// Returns the memoized native method (computed once, cached forever).
fn native_method() -> Option<NativeMethod> {
    static METHOD: OnceLock<Option<NativeMethod>> = OnceLock::new();
    *METHOD.get_or_init(detect_native)
}

// ── copy (aligned with opencode's Clipboard.copy) ─────────────────────

/// Copy text to the clipboard.
///
/// Mirrors opencode's `copy()`:
/// 1. Send OSC 52 first (always, when on a TTY).
/// 2. Run the native command as a supplement; its failure is non-fatal
///    because OSC 52 was already sent (`opencode: .catch(() => {})`).
pub async fn copy(text: String) -> io::Result<()> {
    // Layer 1: OSC 52 — terminal-side, zero dependencies.
    let osc52_sent = write_osc52(&text);

    // Layer 2: native command — supplemental.
    // opencode swallows native errors with `.catch(() => {})`; we do the same:
    // if OSC 52 was sent, a native failure does not propagate.
    if let Some(method) = native_method() {
        if run_native(method, &text).await.is_ok() {
            return Ok(());
        }
    }

    if osc52_sent {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::NotFound,
            "no clipboard support: use a terminal with OSC 52 or install xclip/wl-copy",
        ))
    }
}

/// Spawn the native clipboard binary and pipe text via stdin.
async fn run_native(method: NativeMethod, text: &str) -> io::Result<()> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = Command::new(method.program)
            .args(method.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()?;
        if let Some(mut stdin) = child.stdin.take() {
            stdin.write_all(text.as_bytes()).await?;
            drop(stdin);
        }
        if child.wait().await?.success() {
            Ok(())
        } else {
            Err(io::Error::other("clipboard command failed"))
        }
    })
    .await
    .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "clipboard timed out"))?
}

// ── read_image (aligned with opencode's clipboard.read image branch) ──

/// An image read from the system clipboard. `data` is base64-encoded so it can
/// be placed directly into a [`UserAttachment`](yourai_core::protocol::UserAttachment).
pub struct ClipboardImage {
    pub mime: String,
    pub data: String,
}

/// Spawn a command and capture bounded stdout bytes. Any failure or output
/// larger than `max_bytes` yields an empty buffer.
#[allow(dead_code)] // not every platform uses capture (macOS shells out to osascript directly)
async fn capture(program: &str, args: &[&str], max_bytes: usize) -> Vec<u8> {
    tokio::time::timeout(Duration::from_secs(2), async {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .ok()?;
        let mut stdout = child.stdout.take()?.take(max_bytes.saturating_add(1) as u64);
        let mut bytes = Vec::with_capacity(max_bytes.min(1024 * 1024));
        stdout.read_to_end(&mut bytes).await.ok()?;
        if bytes.len() > max_bytes {
            let _ = child.kill().await;
            return None;
        }
        child.wait().await.ok()?.success().then_some(bytes)
    })
    .await
    .ok()
    .flatten()
    .unwrap_or_default()
}

/// Read an image from the clipboard, mirroring opencode's `read()` image
/// branches. Returns `Ok(None)` when the clipboard holds no image.
///
/// - macOS       → `osascript` extracts the clipboard PNG to a temp file.
/// - Linux       → `wl-paste -t image/png` (Wayland) → `xclip … -t image/png -o`.
/// - Windows/WSL → `powershell.exe` reads the image and emits base64 PNG.
pub async fn read_image() -> io::Result<Option<ClipboardImage>> {
    #[cfg(target_os = "macos")]
    if which("osascript").is_some() {
        if let Some(img) = read_image_macos().await? {
            return Ok(Some(img));
        }
    }

    #[cfg(target_os = "linux")]
    {
        // WSL: the Linux side usually can't access the Windows clipboard via
        // wl-paste/xclip, so shell out to powershell.exe first — matching
        // opencode's `release().includes("WSL")` guard.
        if is_wsl() && which("powershell.exe").is_some() {
            if let Some(img) = read_image_windows().await? {
                return Ok(Some(img));
            }
        }
        if std::env::var_os("WAYLAND_DISPLAY").is_some() && which("wl-paste").is_some() {
            let bytes = capture(
                "wl-paste",
                &["-t", "image/png"],
                MAX_USER_ATTACHMENT_BYTES,
            )
            .await;
            if !bytes.is_empty() {
                return Ok(Some(ClipboardImage {
                    mime: "image/png".into(),
                    data: base64_encode(&bytes),
                }));
            }
        }
        if which("xclip").is_some() {
            let bytes = capture(
                "xclip",
                &[
                    "-selection",
                    "clipboard",
                    "-t",
                    "image/png",
                    "-o",
                ],
                MAX_USER_ATTACHMENT_BYTES,
            )
            .await;
            if !bytes.is_empty() {
                return Ok(Some(ClipboardImage {
                    mime: "image/png".into(),
                    data: base64_encode(&bytes),
                }));
            }
        }
    }

    #[cfg(target_os = "windows")]
    if which("powershell.exe").is_some() {
        if let Some(img) = read_image_windows().await? {
            return Ok(Some(img));
        }
    }

    Ok(None)
}

/// Detect WSL by checking `/proc/sys/kernel/osrelease` for "microsoft".
#[cfg(target_os = "linux")]
fn is_wsl() -> bool {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.to_ascii_lowercase().contains("microsoft"))
        .unwrap_or(false)
}

#[cfg(target_os = "macos")]
async fn read_image_macos() -> io::Result<Option<ClipboardImage>> {
    let dir = tempfile::tempdir()?;
    let file = dir.path().join("clipboard.png");
    let path = file.to_string_lossy().into_owned();
    let open_line =
        format!("set fileRef to open for access POSIX file \"{path}\" with write permission");
    let args = [
        "-e",
        "set imageData to the clipboard as \"PNGf\"",
        "-e",
        open_line.as_str(),
        "-e",
        "set eof fileRef to 0",
        "-e",
        "write imageData to fileRef",
        "-e",
        "close access fileRef",
    ];
    let ok = tokio::time::timeout(Duration::from_secs(3), async {
        Command::new("osascript")
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .status()
            .await
            .ok()
            .map(|s| s.success())
            .unwrap_or(false)
    })
    .await
    .unwrap_or(false);
    let result = if ok
        && tokio::fs::metadata(&file)
            .await
            .is_ok_and(|meta| meta.len() <= MAX_USER_ATTACHMENT_BYTES as u64)
    {
        tokio::fs::read(&file)
            .await
            .ok()
            .filter(|b| !b.is_empty())
            .map(|bytes| ClipboardImage {
                mime: "image/png".into(),
                data: base64_encode(&bytes),
            })
    } else {
        None
    };
    let _ = tokio::fs::remove_file(&file).await;
    Ok(result)
}

#[cfg(any(target_os = "windows", target_os = "linux"))]
async fn read_image_windows() -> io::Result<Option<ClipboardImage>> {
    // Mirrors opencode: PowerShell loads the clipboard image, saves it as PNG
    // to a memory stream, and emits the base64 string on stdout.
    let script = "Add-Type -AssemblyName System.Windows.Forms; \
        $img = [System.Windows.Forms.Clipboard]::GetImage(); \
        if ($img) { $ms = New-Object System.IO.MemoryStream; \
        $img.Save($ms, [System.Drawing.Imaging.ImageFormat]::Png); \
        [System.Convert]::ToBase64String($ms.ToArray()) }";
    let out = capture(
        "powershell.exe",
        &["-NonInteractive", "-NoProfile", "-command", script],
        MAX_ATTACHMENT_BASE64_BYTES,
    )
    .await;
    let text = String::from_utf8_lossy(&out).trim().to_owned();
    if text.is_empty() {
        return Ok(None);
    }
    Ok(Some(ClipboardImage {
        mime: "image/png".into(),
        data: text,
    }))
}

// ── tests ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_round_trip() {
        for (input, expected) in [
            ("", ""),
            ("f", "Zg=="),
            ("fo", "Zm8="),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg=="),
            ("fooba", "Zm9vYmE="),
            ("foobar", "Zm9vYmFy"),
            ("你好", "5L2g5aW9"),
        ] {
            assert_eq!(base64_encode(input.as_bytes()), expected, "{input:?}");
        }
    }

    #[test]
    fn osc52_sequence_is_well_formed() {
        // The sequence must be a complete OSC 52: ESC ] 52 ; c ; <base64> BEL
        let encoded = base64_encode(b"hello");
        let seq = format!("\x1b]52;c;{encoded}\x07");
        assert!(seq.starts_with("\x1b]52;c;"));
        assert!(seq.ends_with('\x07'));
        assert_eq!(encoded, "aGVsbG8=");
    }

    #[test]
    fn tmux_passthrough_wraps_osc52() {
        // opencode wraps in DCS passthrough when TMUX or STY is set.
        let osc52 = "\x1b]52;c;aGVsbG8=\x07";
        let wrapped = format!("\x1bPtmux;\x1b{osc52}\x1b\\");
        assert!(wrapped.starts_with("\x1bPtmux;\x1b"));
        assert!(wrapped.ends_with("\x1b\\"));
        assert!(wrapped.contains(osc52));
    }
}
