use crate::tools::*;
use serde::Deserialize;
use std::{process::Stdio, time::Duration};
#[cfg(unix)]
use tokio::io::AsyncReadExt;

pub struct Shell {
    cwd: PathBuf,
}
impl Shell {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
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
impl ToolHandler for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn definition(&self) -> Tool {
        schema(self.name(),"Execute a foreground command in a fresh /bin/sh process. Use for rg, ls, git, builds and tests. No persistent cwd/environment, interactive input, PTY or background sessions. Inspect exit_code, termination and output_complete. timeout_ms defaults to 120000; maximum 600000 (also bounded by the host).",json!({"command":{"type":"string","minLength":1},"cwd":{"type":"string"},"timeout_ms":{"type":"integer","minimum":1,"maximum":MAX_SHELL_TIMEOUT_MS}}), &["command"])
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
            let i: Input = serde_json::from_value(input).map_err(|e| error(self.name(), e))?;
            let timeout = i.timeout_ms.unwrap_or(SHELL_TIMEOUT_MS);
            if i.command.trim().is_empty() || !(1..=MAX_SHELL_TIMEOUT_MS).contains(&timeout) {
                return Err(error(self.name(), "invalid command or timeout_ms"));
            }
            let cwd = resolve_path(&self.cwd, Path::new(i.cwd.as_deref().unwrap_or(".")))
                .map_err(|e| error(self.name(), e))?;
            if !cwd.is_dir() {
                return Err(error(self.name(), "cwd must be an existing directory"));
            }
            check_cancel(&tc)?;
            if let Some(security) = &tc.security {
                if !matches!(
                    security.check_command(&i.command).await?,
                    PolicyDecision::Allow
                ) {
                    return Err(error(self.name(), "command denied by hard policy"));
                }
            }
            #[cfg(unix)]
            {
                run(tc, &i.command, &cwd, Duration::from_millis(timeout)).await
            }
            #[cfg(not(unix))]
            {
                let _ = (tc, cwd);
                Err(error(
                    self.name(),
                    "shell currently requires macOS or Linux",
                ))
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
    let (mut stdout, mut stderr) = (Vec::new(), Vec::new());
    let (mut obuf, mut ebuf) = ([0u8; 8192], [0u8; 8192]);
    let (mut oeof, mut eeof) = (false, false);
    let (mut opos, mut epos) = (0, 0);
    let mut status = None;
    let mut ticks = tokio::time::interval(Duration::from_millis(100));
    ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    let deadline = tokio::time::sleep(timeout);
    tokio::pin!(deadline);
    let termination = loop {
        if status.is_some() && oeof && eeof {
            break "exit";
        }
        tokio::select! {
            biased;
            _=tc.cancel.cancelled()=>return Err(AbortReason::Cancelled.into()),
            _=tc.emit.closed()=>return Err(AbortReason::Disconnected.into()),
            _=&mut deadline=>break "timeout",
            _=ticks.tick()=>{
                if stdout.len()>opos || stderr.len()>epos {
                    if !tc.emit_progress(json!({"stdout":String::from_utf8_lossy(&stdout[opos..]),"stderr":String::from_utf8_lossy(&stderr[epos..])})) {return Err(AbortReason::Disconnected.into());}
                    opos=stdout.len(); epos=stderr.len();
                }
            },
            r=out.read(&mut obuf),if !oeof=>{
                let n=r.map_err(|e|error("shell",e))?; oeof=n==0;
                let keep=n.min(MAX_OUTPUT_BYTES-stdout.len()-stderr.len()); stdout.extend_from_slice(&obuf[..keep]);
                if keep<n {break "output_limit";}
            },
            r=err.read(&mut ebuf),if !eeof=>{
                let n=r.map_err(|e|error("shell",e))?; eeof=n==0;
                let keep=n.min(MAX_OUTPUT_BYTES-stdout.len()-stderr.len()); stderr.extend_from_slice(&ebuf[..keep]);
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
    Ok(
        json!({"ok":termination=="exit" && status.success(),"exit_code":status.code(),"stdout":String::from_utf8_lossy(&stdout),"stderr":String::from_utf8_lossy(&stderr),"termination":termination,"output_complete":oeof && eeof,"cwd":cwd_str}),
    )
}
