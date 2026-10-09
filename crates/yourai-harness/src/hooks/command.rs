//! Command hook 执行器：spawn 子进程，stdin 写入 JSON+`\n`，读 stdout/stderr，按 exit code 分支。
//!
//! 与 Claude Code command hook 的基础单次请求 transport 对齐：
//! - stdin: `JSON + "\n"` 然后关闭
//! - stdout: 以 `{` 开头 → JSON 解析；否则 plain text
//! - exit code: `0` = 成功；`2` = blocking（stderr = 原因）；其他非零 = non-blocking error
//!
//! Claude 部分调用路径支持的同进程双向 prompt request 尚未在本 transport 实现。

use crate::hooks::event::to_wire_json;
use std::process::Stdio;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};

pub use yourai_core::hooks::HookBackgroundEvent as BackgroundHookEvent;
pub(crate) type BackgroundTasks = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, Vec<tokio::task::JoinHandle<()>>>>,
>;

#[derive(Clone)]
pub(crate) struct BackgroundCommandContext {
    pub tasks: BackgroundTasks,
    pub hook_id: String,
    pub rewake: bool,
    pub timeout: Option<std::time::Duration>,
    pub force_background: bool,
    pub sender: tokio::sync::broadcast::Sender<BackgroundHookEvent>,
}

// Own the process group across foreground/background handoff and future cancellation.
struct OwnedChild {
    child: Option<tokio::process::Child>,
    group: Option<u32>,
    stdin_tasks: tokio::task::JoinSet<std::io::Result<()>>,
}
impl std::ops::Deref for OwnedChild {
    type Target = tokio::process::Child;
    fn deref(&self) -> &Self::Target {
        self.child.as_ref().unwrap()
    }
}
impl std::ops::DerefMut for OwnedChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.child.as_mut().unwrap()
    }
}
impl OwnedChild {
    /// Read both pipes concurrently (each capped) and reap the child.
    async fn read_output(&mut self) -> std::io::Result<(String, String, std::process::ExitStatus)> {
        let mut stdout = self.stdout.take().ok_or_else(|| pipe_error("stdout"))?;
        let mut stderr = self.stderr.take().ok_or_else(|| pipe_error("stderr"))?;
        let (stdout, stderr, input) = tokio::join!(
            read_capped(&mut stdout),
            read_capped(&mut stderr),
            finish_stdin(&mut self.stdin_tasks),
        );
        let status = self.wait().await?;
        input?;
        Ok((stdout?, stderr?, status))
    }
}
async fn finish_stdin(
    tasks: &mut tokio::task::JoinSet<std::io::Result<()>>,
) -> std::io::Result<()> {
    match tasks.join_next().await {
        Some(result) => result.map_err(std::io::Error::other)?,
        None => Ok(()),
    }
}
fn pipe_error(which: &str) -> std::io::Error {
    std::io::Error::other(format!("hook {which} pipe unavailable"))
}

async fn read_capped<R: tokio::io::AsyncRead + Unpin>(reader: &mut R) -> std::io::Result<String> {
    read_with_budget(reader, crate::hooks::MAX_OUTPUT_BYTES).await
}

fn output_limit_error() -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, "hook output exceeds 1 MiB")
}

fn decode_output(bytes: Vec<u8>) -> std::io::Result<String> {
    String::from_utf8(bytes)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))
}

/// Keep draining both pipes so a full pipe cannot block the child, but reject
/// oversized output instead of interpreting a truncated protocol response.
async fn read_with_budget<R: tokio::io::AsyncRead + Unpin>(
    reader: &mut R,
    budget: usize,
) -> std::io::Result<String> {
    let mut out = Vec::new();
    let mut chunk = [0u8; 8192];
    let mut overflow = false;
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return if overflow {
                Err(output_limit_error())
            } else {
                decode_output(out)
            };
        }
        let room = budget.saturating_sub(out.len());
        overflow |= n > room;
        out.extend_from_slice(&chunk[..n.min(room)]);
    }
}

/// The async handshake shares stdout's cap. Never allocate an unbounded first
/// line while waiting to decide whether the process should run in background.
async fn read_first_line<R: AsyncBufRead + Unpin>(reader: &mut R) -> std::io::Result<String> {
    let mut line = Vec::new();
    loop {
        let buffer = reader.fill_buf().await?;
        if buffer.is_empty() {
            return decode_output(line);
        }
        let newline = buffer.iter().position(|byte| *byte == b'\n');
        let take = newline.map_or(buffer.len(), |index| index + 1);
        if take > crate::hooks::MAX_OUTPUT_BYTES.saturating_sub(line.len()) {
            return Err(output_limit_error());
        }
        line.extend_from_slice(&buffer[..take]);
        reader.consume(take);
        if newline.is_some() {
            return decode_output(line);
        }
    }
}
impl Drop for OwnedChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.group {
            // SAFETY: the group ID belongs to the child we spawned with process_group(0).
            unsafe {
                libc::kill(-(pid as i32), libc::SIGKILL);
            }
        }
    }
}

/// Command hook handler。
pub struct CommandHandler {
    command: String,
    shell: Option<crate::hooks::config::HookShell>,
    background: Option<BackgroundCommandContext>,
}

impl CommandHandler {
    pub fn new(command: String, shell: Option<crate::hooks::config::HookShell>) -> Self {
        Self {
            command,
            shell,
            background: None,
        }
    }

    pub(crate) fn with_background(mut self, background: BackgroundCommandContext) -> Self {
        self.background = Some(background);
        self
    }

    /// 执行 command，返回 (stdout, stderr, exit_code)。
    async fn spawn(
        &self,
        invocation: &HookInvocation,
    ) -> Result<OwnedChild, yourai_core::YourAiError> {
        let json_input = to_wire_json(invocation);

        let mut cmd = match self.shell {
            Some(crate::hooks::config::HookShell::Powershell) => {
                let mut command = Command::new("pwsh");
                command
                    .arg("-NoProfile")
                    .arg("-NonInteractive")
                    .arg("-Command")
                    .arg(&self.command);
                command
            }
            Some(crate::hooks::config::HookShell::Bash) => {
                let mut command = Command::new("bash");
                command.arg("-c").arg(&self.command);
                command
            }
            None => {
                // Fixed fallback: $SHELL is user-controlled and can point anywhere.
                let mut command = Command::new("/bin/sh");
                command.arg("-c").arg(&self.command);
                command
            }
        };
        cmd.stdin(Stdio::piped());
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());
        cmd.kill_on_drop(true);
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.as_std_mut().process_group(0);
        }
        cmd.current_dir(&invocation.base.cwd);

        let child = cmd.spawn().map_err(|e| yourai_core::ErrorKind::Provider {
            name: "hook",
            message: format!("failed to spawn command: {e}"),
        })?;

        let mut child = OwnedChild {
            group: child.id(),
            child: Some(child),
            stdin_tasks: tokio::task::JoinSet::new(),
        };
        if let Some(mut stdin) = child.stdin.take() {
            // Keep this writer owned by the process while stdout/stderr drain.
            // Awaiting it here can deadlock against a hook echoing a large input.
            child.stdin_tasks.spawn(async move {
                let write = async {
                    stdin
                        .write_all(format!("{json_input}\n").as_bytes())
                        .await?;
                    stdin.flush().await
                }
                .await;
                // Hooks may decide without reading stdin. An early close must not
                // discard their stdout or exit-2 denial; still reap and interpret
                // the process below. Other I/O failures remain errors.
                if let Err(e) = write {
                    if e.kind() != std::io::ErrorKind::BrokenPipe {
                        return Err(e);
                    }
                }
                Ok(())
            });
        }

        Ok(child)
    }

    async fn run(
        &self,
        invocation: &HookInvocation,
    ) -> Result<(String, String, i32), yourai_core::YourAiError> {
        let mut child = self.spawn(invocation).await?;
        let (stdout, stderr, status) =
            child
                .read_output()
                .await
                .map_err(|e| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("wait output: {e}"),
                })?;
        Ok((stdout, stderr, status.code().unwrap_or(-1)))
    }

    async fn run_with_async_detection(
        &self,
        invocation: &HookInvocation,
        background: BackgroundCommandContext,
    ) -> Result<HookOutput, yourai_core::YourAiError> {
        if background.force_background {
            let child = self.spawn(invocation).await?;
            return Ok(background_child(child, background, invocation, None));
        }

        let mut child = self.spawn(invocation).await?;
        let stdout = child.stdout.take().ok_or_else(|| {
            yourai_core::YourAiError::from(yourai_core::ErrorKind::Provider {
                name: "hook",
                message: "hook stdout pipe unavailable".to_string(),
            })
        })?;
        let stderr = child.stderr.take().ok_or_else(|| {
            yourai_core::YourAiError::from(yourai_core::ErrorKind::Provider {
                name: "hook",
                message: "hook stderr pipe unavailable".to_string(),
            })
        })?;

        let mut stdout = BufReader::new(stdout);
        let stderr_task = tokio::spawn(async move {
            let mut stderr = stderr;
            read_capped(&mut stderr).await
        });
        let mut first_line = read_first_line(&mut stdout).await.map_err(|error| {
            yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed reading hook stdout: {error}"),
            }
        })?;

        let async_response = match crate::hooks::wire_output::parse_hook_json(first_line.trim()) {
            Ok(crate::hooks::wire_output::HookJsonOutput::Async(output)) => Some(output),
            _ => None,
        };
        if let Some(async_response) = async_response {
            let async_timeout = async_response
                .async_timeout
                .map(std::time::Duration::from_millis);
            let task_id = format!("async_hook_{}", uuid::Uuid::new_v4());
            let event_task_id = task_id.clone();
            let session_id = invocation.base.session_id.clone();
            let event_name = invocation.event_kind().as_str().to_owned();
            let owner = session_id.clone();
            let tasks = background.tasks.clone();
            let task = tokio::spawn(async move {
                let stdout_task = tokio::spawn(async move {
                    let mut stdout = stdout;
                    read_capped(&mut stdout).await
                });
                let wait_result = match async_timeout.or(background.timeout) {
                    Some(timeout) => match tokio::time::timeout(timeout, child.wait()).await {
                        Ok(result) => result.map(Some),
                        Err(_) => {
                            let _ = child.kill().await;
                            let _ = child.wait().await;
                            Ok(None)
                        }
                    },
                    None => child.wait().await.map(Some),
                };
                let mut stdin_tasks = std::mem::take(&mut child.stdin_tasks);
                drop(child); // End the owned process group before joining pipe readers.
                let input_result = finish_stdin(&mut stdin_tasks).await;
                let (read_result, remaining) = match stdout_task.await {
                    Ok(Ok(text)) => (Ok(()), text),
                    Ok(Err(error)) => (Err(error), String::new()),
                    Err(error) => (Ok(()), format!("failed joining stdout reader: {error}")),
                };
                let mut stderr = match stderr_task.await {
                    Ok(Ok(text)) => text,
                    Ok(Err(error)) => format!("failed reading stderr: {error}"),
                    Err(error) => format!("failed reading stderr: {error}"),
                };
                if let Err(error) = input_result {
                    stderr.push_str(&format!("\nstdin write: {error}"));
                }
                if let Err(error) = read_result {
                    stderr.push_str(&format!("\nfailed reading stdout: {error}"));
                }
                let (exit_code, timed_out, extra_error) = match wait_result {
                    Ok(Some(status)) => (status.code().unwrap_or(-1), false, None),
                    Ok(None) => (-1, true, Some("background hook timed out".to_string())),
                    Err(error) => (-1, false, Some(error.to_string())),
                };
                if let Some(error) = extra_error {
                    stderr.push_str(&format!("\n{error}"));
                }
                let _ = background.sender.send(BackgroundHookEvent {
                    session_id,
                    task_id: event_task_id,
                    hook_id: background.hook_id,
                    event_name,
                    stdout: remaining,
                    stderr,
                    exit_code,
                    timed_out,
                    rewake: background.rewake,
                });
            });
            {
                let mut registry = tasks.lock().unwrap();
                let owned = registry.entry(owner).or_default();
                owned.retain(|task| !task.is_finished());
                owned.push(task);
            }
            return Ok(HookOutput::Backgrounded { task_id });
        }

        let remaining = read_with_budget(
            &mut stdout,
            crate::hooks::MAX_OUTPUT_BYTES - first_line.len(),
        )
        .await
        .map_err(|error| yourai_core::ErrorKind::Provider {
            name: "hook",
            message: format!("failed reading hook stdout: {error}"),
        })?;
        let status = child
            .wait()
            .await
            .map_err(|error| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed waiting for hook: {error}"),
            })?;
        finish_stdin(&mut child.stdin_tasks)
            .await
            .map_err(|error| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("stdin write: {error}"),
            })?;
        let stderr = stderr_task
            .await
            .map_err(|error| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed joining stderr reader: {error}"),
            })?
            .map_err(|error| yourai_core::ErrorKind::Provider {
                name: "hook",
                message: format!("failed reading hook stderr: {error}"),
            })?;
        first_line.push_str(&remaining);
        Ok(HookOutput::Command {
            stdout: first_line,
            stderr,
            exit_code: status.code().unwrap_or(-1),
        })
    }
}

fn background_child(
    child: OwnedChild,
    background: BackgroundCommandContext,
    invocation: &HookInvocation,
    timeout_override: Option<std::time::Duration>,
) -> HookOutput {
    let task_id = format!("async_hook_{}", uuid::Uuid::new_v4());
    let event_task_id = task_id.clone();
    let session_id = invocation.base.session_id.clone();
    let event_name = invocation.event_kind().as_str().to_owned();
    let owner = session_id.clone();
    let tasks = background.tasks.clone();
    let task = tokio::spawn(async move {
        let mut child = child;
        let read = child.read_output();
        let result = match timeout_override.or(background.timeout) {
            Some(timeout) => match tokio::time::timeout(timeout, read).await {
                Ok(result) => result.map(Some),
                Err(_) => Ok(None),
            },
            None => read.await.map(Some),
        };
        let event = match result {
            Ok(Some((stdout, stderr, status))) => BackgroundHookEvent {
                session_id,
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name,
                stdout,
                stderr,
                exit_code: status.code().unwrap_or(-1),
                timed_out: false,
                rewake: background.rewake,
            },
            Ok(None) => BackgroundHookEvent {
                session_id,
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name,
                stdout: String::new(),
                stderr: "background hook timed out".to_string(),
                exit_code: -1,
                timed_out: true,
                rewake: background.rewake,
            },
            Err(error) => BackgroundHookEvent {
                session_id,
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name,
                stdout: String::new(),
                stderr: error.to_string(),
                exit_code: -1,
                timed_out: false,
                rewake: background.rewake,
            },
        };
        let _ = background.sender.send(event);
    });
    {
        let mut registry = tasks.lock().unwrap();
        let owned = registry.entry(owner).or_default();
        owned.retain(|task| !task.is_finished());
        owned.push(task);
    }
    HookOutput::Backgrounded { task_id }
}

impl HookHandler for CommandHandler {
    fn execute<'a>(
        &'a self,
        invocation: &'a HookInvocation,
    ) -> yourai_core::BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
        Box::pin(async move {
            if let Some(background) = self.background.clone() {
                return self.run_with_async_detection(invocation, background).await;
            }
            let (stdout, stderr, exit_code) = self.run(invocation).await?;
            Ok(HookOutput::Command {
                stdout,
                stderr,
                exit_code,
            })
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yourai_core::hooks::{BaseInput, HookEvent};

    #[tokio::test]
    async fn command_exit_zero_empty() {
        let handler = CommandHandler::new("echo -n ''".to_string(), None);
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = handler.execute(&inv).await.unwrap();
        match result {
            HookOutput::Command { exit_code, .. } => assert_eq!(exit_code, 0),
            _ => panic!("expected command output"),
        }
    }

    #[tokio::test]
    async fn command_exit_two_blocking() {
        let handler = CommandHandler::new("echo 'blocked' >&2; exit 2".to_string(), None);
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = handler.execute(&inv).await.unwrap();
        match result {
            HookOutput::Command {
                exit_code, stderr, ..
            } => {
                assert_eq!(exit_code, 2);
                assert!(stderr.contains("blocked"));
            }
            _ => panic!("expected command output"),
        }
    }

    #[tokio::test]
    async fn early_stdin_close_preserves_hook_exit_status_and_output() {
        let handler = CommandHandler::new(
            "exec 0<&-; echo blocked >&2; exit 2".into(),
            Some(crate::hooks::config::HookShell::Bash),
        );
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "x".repeat(1024 * 1024),
            },
        );
        match handler.execute(&inv).await.unwrap() {
            HookOutput::Command {
                exit_code, stderr, ..
            } => {
                assert_eq!(exit_code, 2);
                assert!(stderr.contains("blocked"));
            }
            _ => panic!("expected command output"),
        }
    }

    #[tokio::test]
    async fn command_reads_stdin() {
        let handler = CommandHandler::new("cat".to_string(), None);
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::PreToolUse {
                tool_name: "Bash".to_string(),
                tool_input: serde_json::json!({"command": "ls"}),
                tool_use_id: "call_1".to_string(),
            },
        );
        let result = handler.execute(&inv).await.unwrap();
        match result {
            HookOutput::Command { stdout, .. } => {
                assert!(stdout.contains("hook_event_name"));
                assert!(stdout.contains("PreToolUse"));
                assert!(stdout.contains("Bash"));
            }
            _ => panic!("expected command output"),
        }
    }

    #[tokio::test]
    async fn large_input_and_output_are_drained_concurrently() {
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "x".repeat(262_144),
            },
        );
        let expected = format!("{}\n", to_wire_json(&inv));
        for command in ["cat", "cat >&2"] {
            let handler = CommandHandler::new(command.into(), None);
            match tokio::time::timeout(std::time::Duration::from_secs(2), handler.execute(&inv))
                .await
                .expect("writing stdin must not block output drainage")
                .unwrap()
            {
                HookOutput::Command {
                    stdout,
                    stderr,
                    exit_code,
                } => {
                    assert_eq!(exit_code, 0);
                    assert_eq!(if command == "cat" { stdout } else { stderr }, expected);
                }
                _ => panic!("expected foreground output"),
            }
        }
    }

    #[tokio::test]
    async fn large_input_survives_async_detection_and_handoff() {
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "x".repeat(262_144),
            },
        );
        let expected = format!("{}\n", to_wire_json(&inv));
        for command in ["cat >&2", "printf '{\"async\":true}\\n'; cat >&2"] {
            let (sender, mut events) = tokio::sync::broadcast::channel(4);
            let tasks = BackgroundTasks::default();
            let handler = CommandHandler::new(command.into(), None).with_background(
                BackgroundCommandContext {
                    tasks: tasks.clone(),
                    hook_id: "large-input".into(),
                    rewake: false,
                    timeout: Some(std::time::Duration::from_secs(2)),
                    force_background: false,
                    sender,
                },
            );
            let output =
                tokio::time::timeout(std::time::Duration::from_secs(2), handler.execute(&inv))
                    .await
                    .unwrap()
                    .unwrap();
            if command == "cat >&2" {
                match output {
                    HookOutput::Command {
                        stderr, exit_code, ..
                    } => {
                        assert_eq!(exit_code, 0);
                        assert_eq!(stderr, expected);
                    }
                    _ => panic!("expected foreground output"),
                }
            } else {
                assert!(matches!(output, HookOutput::Backgrounded { .. }));
                let event = tokio::time::timeout(std::time::Duration::from_secs(2), events.recv())
                    .await
                    .unwrap()
                    .unwrap();
                assert_eq!(event.exit_code, 0);
                assert!(!event.timed_out);
                assert_eq!(event.stderr, expected);
                let handles = tasks.lock().unwrap().remove("sess").unwrap();
                for handle in handles {
                    handle.await.unwrap();
                }
            }
        }
    }

    #[tokio::test]
    async fn explicit_bash_shell_does_not_depend_on_shell_environment() {
        let handler = CommandHandler::new(
            "printf %s \"$BASH_VERSION\"".to_string(),
            Some(crate::hooks::config::HookShell::Bash),
        );
        let inv = HookInvocation::new(
            BaseInput::new("sess", "/tmp"),
            HookEvent::UserPromptSubmit {
                prompt: "hi".to_string(),
            },
        );
        let result = handler.execute(&inv).await.unwrap();
        match result {
            HookOutput::Command {
                stdout, exit_code, ..
            } => {
                assert_eq!(exit_code, 0);
                assert!(!stdout.is_empty());
            }
            _ => panic!("expected command output"),
        }
    }
}
