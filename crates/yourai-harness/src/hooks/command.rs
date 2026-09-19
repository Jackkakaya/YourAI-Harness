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
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;
use yourai_core::hooks::{HookHandler, HookInvocation, HookOutput};

pub use yourai_core::hooks::HookBackgroundEvent as BackgroundHookEvent;
pub(crate) type BackgroundTasks = std::sync::Arc<
    std::sync::Mutex<std::collections::HashMap<String, Vec<tokio::task::JoinHandle<()>>>>,
>;

#[derive(Clone)]
pub(crate) struct BackgroundCommandContext {
    pub session_id: String,
    pub tasks: BackgroundTasks,
    pub hook_id: String,
    pub event_name: String,
    pub rewake: bool,
    pub timeout: Option<std::time::Duration>,
    pub force_background: bool,
    pub sender: tokio::sync::broadcast::Sender<BackgroundHookEvent>,
}

// Own the process group across foreground/background handoff and future cancellation.
struct OwnedChild {
    child: Option<tokio::process::Child>,
    group: Option<u32>,
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
    async fn wait_with_output(mut self) -> std::io::Result<std::process::Output> {
        self.child.take().unwrap().wait_with_output().await
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
                let shell = std::env::var("SHELL").unwrap_or_else(|_| "sh".to_string());
                let mut command = Command::new(shell);
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
        };
        if let Some(mut stdin) = child.stdin.take() {
            stdin
                .write_all(format!("{json_input}\n").as_bytes())
                .await
                .map_err(|e| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("stdin write: {e}"),
                })?;
            stdin
                .flush()
                .await
                .map_err(|e| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("stdin flush: {e}"),
                })?;
        }

        Ok(child)
    }

    async fn run(
        &self,
        invocation: &HookInvocation,
    ) -> Result<(String, String, i32), yourai_core::YourAiError> {
        let child = self.spawn(invocation).await?;
        let output =
            child
                .wait_with_output()
                .await
                .map_err(|e| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("wait output: {e}"),
                })?;

        let stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let exit_code = output.status.code().unwrap_or(-1);

        Ok((stdout, stderr, exit_code))
    }

    async fn run_with_async_detection(
        &self,
        invocation: &HookInvocation,
        background: BackgroundCommandContext,
    ) -> Result<HookOutput, yourai_core::YourAiError> {
        if background.force_background {
            let child = self.spawn(invocation).await?;
            return Ok(background_child(child, background, None));
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
        let mut first_line = String::new();
        let stderr_task = tokio::spawn(async move {
            let mut stderr = BufReader::new(stderr);
            let mut value = String::new();
            let result = stderr.read_to_string(&mut value).await;
            (result, value)
        });
        stdout.read_line(&mut first_line).await.map_err(|error| {
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
            let owner = background.session_id.clone();
            let tasks = background.tasks.clone();
            let task = tokio::spawn(async move {
                let stdout_task = tokio::spawn(async move {
                    let mut remaining = String::new();
                    let result = stdout.read_to_string(&mut remaining).await;
                    (result, remaining)
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
                drop(child); // End the owned process group before joining pipe readers.
                let (read_result, remaining) = stdout_task.await.unwrap_or_else(|error| {
                    (Ok(0), format!("failed joining stdout reader: {error}"))
                });
                let (_, stderr) = stderr_task
                    .await
                    .unwrap_or_else(|error| (Ok(0), format!("failed reading stderr: {error}")));
                let (exit_code, timed_out, extra_error) = match wait_result {
                    Ok(Some(status)) => (status.code().unwrap_or(-1), false, None),
                    Ok(None) => (-1, true, Some("background hook timed out".to_string())),
                    Err(error) => (-1, false, Some(error.to_string())),
                };
                let mut stderr = stderr;
                if let Err(error) = read_result {
                    stderr.push_str(&format!("\nfailed reading stdout: {error}"));
                }
                if let Some(error) = extra_error {
                    stderr.push_str(&format!("\n{error}"));
                }
                let _ = background.sender.send(BackgroundHookEvent {
                    session_id: background.session_id.clone(),
                    task_id: event_task_id,
                    hook_id: background.hook_id,
                    event_name: background.event_name,
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

        let mut remaining = String::new();
        stdout
            .read_to_string(&mut remaining)
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
        let (stderr_result, stderr) =
            stderr_task
                .await
                .map_err(|error| yourai_core::ErrorKind::Provider {
                    name: "hook",
                    message: format!("failed joining stderr reader: {error}"),
                })?;
        stderr_result.map_err(|error| yourai_core::ErrorKind::Provider {
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
    timeout_override: Option<std::time::Duration>,
) -> HookOutput {
    let task_id = format!("async_hook_{}", uuid::Uuid::new_v4());
    let event_task_id = task_id.clone();
    let owner = background.session_id.clone();
    let tasks = background.tasks.clone();
    let task = tokio::spawn(async move {
        let wait = child.wait_with_output();
        let result = match timeout_override.or(background.timeout) {
            Some(timeout) => match tokio::time::timeout(timeout, wait).await {
                Ok(result) => result.map(Some),
                Err(_) => Ok(None),
            },
            None => wait.await.map(Some),
        };
        let event = match result {
            Ok(Some(output)) => BackgroundHookEvent {
                session_id: background.session_id.clone(),
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name: background.event_name,
                stdout: String::from_utf8_lossy(&output.stdout).to_string(),
                stderr: String::from_utf8_lossy(&output.stderr).to_string(),
                exit_code: output.status.code().unwrap_or(-1),
                timed_out: false,
                rewake: background.rewake,
            },
            Ok(None) => BackgroundHookEvent {
                session_id: background.session_id.clone(),
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name: background.event_name,
                stdout: String::new(),
                stderr: "background hook timed out".to_string(),
                exit_code: -1,
                timed_out: true,
                rewake: background.rewake,
            },
            Err(error) => BackgroundHookEvent {
                session_id: background.session_id.clone(),
                task_id: event_task_id,
                hook_id: background.hook_id,
                event_name: background.event_name,
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
    ) -> crate::hooks::handler::BoxFuture<'a, Result<HookOutput, yourai_core::YourAiError>> {
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
