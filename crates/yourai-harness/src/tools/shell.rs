use crate::tools::*;
use serde::Deserialize;
use std::{process::Stdio, time::Duration};
#[cfg(unix)]
use tokio::io::{AsyncReadExt, AsyncWriteExt};

pub struct Shell {
    cwd: PathBuf,
    output: Option<Arc<ToolOutputStore>>,
}
impl Shell {
    pub fn new(cwd: PathBuf) -> Self {
        Self::with_output(cwd, None)
    }
    pub fn with_output(cwd: PathBuf, output: Option<Arc<ToolOutputStore>>) -> Self {
        Self { cwd, output }
    }
}
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Input {
    command: String,
    cwd: Option<String>,
    timeout_ms: Option<u64>,
}

// Tokio's kill_on_drop covers the immediate child; the group guard covers descendants.
#[cfg(unix)]
struct ProcessGroup(u32);
#[cfg(unix)]
impl Drop for ProcessGroup {
    fn drop(&mut self) {
        // SAFETY: negative PID addresses only the dedicated process group created below.
        unsafe {
            libc::kill(-(self.0 as i32), libc::SIGKILL);
        }
    }
}
impl ToolProvider for Shell {
    fn definition(&self) -> ToolDefinition {
        schema("shell","Execute a foreground command in a fresh /bin/sh process. Use for rg, ls, git, builds and tests. No persistent cwd/environment, interactive input, PTY or background sessions. Inspect exit_code, termination and output_complete. timeout_ms defaults to 120000; maximum 600000 (also bounded by the host).",json!({"command":{"type":"string","minLength":1},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_SHELL_TIMEOUT_MS}}), &["command"])
    }
    fn security_context(&self, input: &Value, cwd: Option<&Path>) -> SecurityContext {
        security("shell", cwd.unwrap_or(&self.cwd), input, true)
    }
    fn run<'a>(
        &'a self,
        tc: ToolContext<'a>,
        input: Value,
    ) -> BoxFuture<'a, Result<Value, YourAiError>> {
        Box::pin(async move {
            let i: Input = serde_json::from_value(input).map_err(|e| error("shell", e))?;
            let timeout = i.timeout_ms.unwrap_or(SHELL_TIMEOUT_MS);
            if i.command.trim().is_empty() || !(1..=MAX_SHELL_TIMEOUT_MS).contains(&timeout) {
                return Err(error("shell", "invalid command or timeout_ms"));
            }
            let cwd = resolve_path(
                tc.cwd.unwrap_or(&self.cwd),
                Path::new(i.cwd.as_deref().unwrap_or(".")),
            )
            .map_err(|e| error("shell", e))?;
            if !cwd.is_dir() {
                return Err(error("shell", "cwd must be an existing directory"));
            }
            check_cancel(&tc)?;
            if let Some(security) = &tc.security {
                if !matches!(
                    security.check_command(&i.command).await?,
                    PolicyDecision::Allow
                ) {
                    return Err(error("shell", "command denied by hard policy"));
                }
            }
            #[cfg(unix)]
            {
                run(
                    tc,
                    &i.command,
                    &cwd,
                    Duration::from_millis(timeout),
                    self.output.clone(),
                )
                .await
            }
            #[cfg(not(unix))]
            {
                let _ = (tc, cwd);
                Err(error("shell", "shell currently requires macOS or Linux"))
            }
        })
    }
}
#[cfg(unix)]
async fn run(
    tc: ToolContext<'_>,
    command: &str,
    cwd: &Path,
    timeout: Duration,
    store: Option<Arc<ToolOutputStore>>,
) -> Result<Value, YourAiError> {
    let cwd_str = utf8_path(cwd, "shell")?;
    let mut cmd = tokio::process::Command::new("/bin/sh");
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    if let Some(sandbox) = &tc.sandbox {
        sandbox.apply(&mut cmd)?;
    }
    cmd.process_group(0);
    check_cancel(&tc)?;
    let mut child = cmd.spawn().map_err(|e| error("shell", e))?;
    let group = ProcessGroup(child.id().expect("spawned child has a pid"));
    let mut out = child.stdout.take().unwrap();
    let mut err = child.stderr.take().unwrap();
    let (mut stdout, mut stderr) = (Capture::new(store.clone()), Capture::new(store.clone()));
    let (mut progress_out, mut progress_err) = (Vec::new(), Vec::new());
    let (mut obuf, mut ebuf) = ([0u8; 8192], [0u8; 8192]);
    let (mut oeof, mut eeof) = (false, false);

    let mut status = None;
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let termination = 'running: loop {
        if status.is_some() && oeof && eeof {
            break "exit";
        }
        tokio::select! {
            biased;
            _=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),
            _=tc.emit.closed()=>return Err(AbortReason::Disconnected.into()),
            _=&mut deadline=>break "timeout",
            _=ticks.tick()=>{
                if !progress_out.is_empty() || !progress_err.is_empty() {
                    if !tc.emit_progress(json!({"stdout":String::from_utf8_lossy(&progress_out),"stderr":String::from_utf8_lossy(&progress_err)})) {return Err(AbortReason::Disconnected.into());}
                    progress_out.clear(); progress_err.clear();
                }
            },
            r=out.read(&mut obuf),if !oeof=>{
                let n=r.map_err(|e|error("shell",e))?; oeof=n==0;
                // Without a managed store preserve the standalone capture ceiling.
                let keep = if store.is_some() { n } else { n.min(MAX_OUTPUT_BYTES-stdout.buffer.len()-stderr.buffer.len()) };
                tokio::select! {
                    biased;
                    _=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),
                    _=tc.emit.closed()=>return Err(AbortReason::Disconnected.into()),
                    _=&mut deadline=>break 'running "timeout",
                    r=stdout.push(&obuf[..keep])=>r?,
                }
                let progress_keep = keep.min(16*1024-progress_out.len());
                progress_out.extend_from_slice(&obuf[..progress_keep]);
                if keep<n {break "output_limit";}
            },
            r=err.read(&mut ebuf),if !eeof=>{
                let n=r.map_err(|e|error("shell",e))?; eeof=n==0;
                // Without a managed store preserve the standalone capture ceiling.
                let keep = if store.is_some() { n } else { n.min(MAX_OUTPUT_BYTES-stdout.buffer.len()-stderr.buffer.len()) };
                tokio::select! {
                    biased;
                    _=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),
                    _=tc.emit.closed()=>return Err(AbortReason::Disconnected.into()),
                    _=&mut deadline=>break 'running "timeout",
                    r=stderr.push(&ebuf[..keep])=>r?,
                }
                let progress_keep = keep.min(16*1024-progress_err.len());
                progress_err.extend_from_slice(&ebuf[..progress_keep]);
                if keep<n {break "output_limit";}
            },
            r=child.wait(),if status.is_none()=>{status=Some(r.map_err(|e|error("shell",e))?);},
        }
    };
    drop(group); // also remove descendants left after a shell exits normally
    if status.is_none() {
        status = Some(child.wait().await.map_err(|e| error("shell", e))?);
    }
    let status = status.unwrap();
    let termination = if termination == "exit" && status.code().is_none() {
        "signal"
    } else {
        termination
    };
    stdout.flush().await?;
    stderr.flush().await?;
    let paths: Vec<_> = [&stdout.path, &stderr.path]
        .into_iter()
        .flatten()
        .map(|p| p.to_string_lossy().into_owned())
        .collect();
    Ok(json!({
        "ok":termination=="exit" && status.success(), "exit_code":status.code(),
        "stdout":stdout.text(), "stderr":stderr.text(), "termination":termination,
        "capture_complete":oeof && eeof, "output_complete":oeof && eeof && paths.is_empty(),
        "output_paths":paths, "stdout_path":stdout.path, "stderr_path":stderr.path, "cwd":cwd_str,
    }))
}

/// Each stream gets a unique file. After spilling, a fixed head and rolling tail remain in memory.
#[cfg(unix)]
struct Capture {
    store: Option<Arc<ToolOutputStore>>,
    buffer: Vec<u8>,
    head: Vec<u8>,
    path: Option<PathBuf>,
    file: Option<tokio::fs::File>,
    lines: usize,
}
#[cfg(unix)]
impl Capture {
    const BYTES: usize = 20 * 1024;
    fn new(store: Option<Arc<ToolOutputStore>>) -> Self {
        Self {
            store,
            buffer: vec![],
            head: vec![],
            path: None,
            file: None,
            lines: 1,
        }
    }
    async fn push(&mut self, bytes: &[u8]) -> Result<(), YourAiError> {
        self.lines = self
            .lines
            .saturating_add(bytes.iter().filter(|b| **b == b'\n').count());
        if self.file.is_none()
            && (self.buffer.len() + bytes.len() > Self::BYTES || self.lines > 1000)
        {
            if let Some(store) = &self.store {
                let (path, mut file) = store.create().await?;
                file.write_all(&self.buffer)
                    .await
                    .map_err(|e| error("shell", e))?;
                self.path = Some(path);
                self.file = Some(file);
            }
        }
        if let Some(file) = &mut self.file {
            file.write_all(bytes).await.map_err(|e| error("shell", e))?;
        }
        self.buffer.extend_from_slice(bytes);
        if self.file.is_some() {
            if self.head.is_empty() {
                let end = (Self::BYTES / 2).min(self.buffer.len());
                // Keep the current bytes available to the rolling tail too.
                // A line-count spill may happen before the byte budget fills.
                self.head.extend_from_slice(&self.buffer[..end]);
            }
            if self.buffer.len() > Self::BYTES / 2 {
                self.buffer.drain(..self.buffer.len() - Self::BYTES / 2);
            }
        }
        Ok(())
    }
    async fn flush(&mut self) -> Result<(), YourAiError> {
        if let Some(file) = &mut self.file {
            file.flush().await.map_err(|e| error("shell", e))?;
        }
        Ok(())
    }
    fn text(&self) -> String {
        match &self.path {
            Some(path) => super::output::preview_parts(
                &head_text(&self.head),
                &tail_text(&self.buffer),
                path.to_str().expect("validated output root"),
                1000,
                Self::BYTES,
            ),
            None => String::from_utf8_lossy(&self.buffer).into_owned(),
        }
    }
}

// Sample boundaries can bisect a UTF-8 character even when the captured stream
// is valid. Omit just that partial character; the disk file retains every byte.
#[cfg(unix)]
fn head_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let bytes = match std::str::from_utf8(bytes) {
        Err(error) if error.error_len().is_none() => &bytes[..error.valid_up_to()],
        _ => bytes,
    };
    String::from_utf8_lossy(bytes)
}
#[cfg(unix)]
fn tail_text(bytes: &[u8]) -> std::borrow::Cow<'_, str> {
    let start = bytes
        .iter()
        .position(|byte| byte & 0xc0 != 0x80)
        .unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[start..])
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn line_count_spill_keeps_head_and_actual_tail_across_chunk_sizes() {
        let root = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::open(root.path().join("output")).unwrap();
        let text = format!(
            "HEAD\n{}MIDDLE\n{}TAIL",
            "x\n".repeat(600),
            "y\n".repeat(600)
        );
        assert!(text.len() < Capture::BYTES / 2);
        for chunk_size in [1, 37, 4096] {
            let mut capture = Capture::new(Some(store.clone()));
            for chunk in text.as_bytes().chunks(chunk_size) {
                capture.push(chunk).await.unwrap();
                assert!(capture.head.len() + capture.buffer.len() <= Capture::BYTES);
            }
            capture.flush().await.unwrap();
            assert_eq!(
                tokio::fs::read(capture.path.as_ref().unwrap())
                    .await
                    .unwrap(),
                text.as_bytes()
            );
            let preview = capture.text();
            assert!(preview.starts_with("HEAD\n"));
            assert!(preview.ends_with("TAIL"));
            assert!(!preview.contains("MIDDLE"));
            assert!(preview.contains("output truncated"));
            assert!(preview.lines().count() <= 1000);
            assert!(preview.len() <= Capture::BYTES);
        }
    }

    #[tokio::test]
    async fn capture_keeps_head_and_tail_with_exact_file_and_unicode_boundaries() {
        let root = tempfile::tempdir().unwrap();
        let store = ToolOutputStore::open(root.path().join("output")).unwrap();
        let mut capture = Capture::new(Some(store));
        let text = format!(
            "HEAD{}MIDDLE{}TAIL",
            "界".repeat(20_000),
            "文".repeat(20_000)
        );
        for chunk in text.as_bytes().chunks(4096) {
            capture.push(chunk).await.unwrap();
            assert!(capture.head.len() + capture.buffer.len() <= Capture::BYTES);
        }
        capture.flush().await.unwrap();
        assert_eq!(
            tokio::fs::read(capture.path.as_ref().unwrap())
                .await
                .unwrap(),
            text.as_bytes()
        );
        let preview = capture.text();
        assert!(preview.starts_with("HEAD"));
        assert!(preview.ends_with("TAIL"));
        assert!(!preview.contains("MIDDLE"));
        assert!(!preview.contains('\u{fffd}'));
        assert!(preview.len() <= Capture::BYTES);
        let (head, tail) = preview.split_once("output truncated").unwrap();
        assert!(head.contains('界'));
        assert!(!head.contains('文'));
        assert!(tail.contains('文'));
    }
}
