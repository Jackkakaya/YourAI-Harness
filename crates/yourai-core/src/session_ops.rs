//! 会话生命周期公共操作（固定模板）。
//!
//! SessionStart / SessionEnd / TurnCompleted 的 hook 编排（派发、消费语义、
//! 超时、错误聚合）在此固定；宿主初始化与资源清理通过回调注入。

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use crate::error::{ErrorKind, YourAiError};
use crate::future::BoxFuture;
use crate::hooks::{HookEvent, HookHost, HookMessageKind, HookRuntime};
use crate::turn::TurnId;

/// SessionStart 结果的宿主应用回调（监视路径、初始输入）。
pub trait SessionStartSink: Send + Sync {
    fn watch_path(&self, path: PathBuf) -> BoxFuture<'_, Result<(), YourAiError>>;
    fn submit_user_text(&self, message: String) -> BoxFuture<'_, Result<(), YourAiError>>;
}

/// 固定公共操作：会话启动 hook 生命周期。
///
/// 派发 SessionStart（不阻断），应用结果中的监视路径与初始用户输入。
pub fn session_start<'a>(
    host: &'a dyn HookHost,
    sink: &'a dyn SessionStartSink,
    source: &'a str,
    model: Option<String>,
) -> BoxFuture<'a, Result<(), YourAiError>> {
    Box::pin(async move {
        let result = host
            .dispatch_hook(HookEvent::SessionStart {
                source: source.into(),
                model,
            })
            .await?;
        host.consume_hook_result(&result, false).await?;
        if let crate::hooks::HookPointOutcome::SessionStart(outcome) = result.outcome {
            for path in outcome.watch_paths {
                sink.watch_path(PathBuf::from(path)).await?;
            }
            if let Some(message) = outcome.initial_user_message {
                sink.submit_user_text(message).await?;
            }
        }
        Ok(())
    })
}

/// 固定公共操作：会话结束 hook 生命周期。
///
/// 派发 SessionEnd（限时、错误不阻断资源释放），通知 HookRuntime 会话关闭，
/// 聚合非阻断错误返回给调用方记录。
pub fn session_end<'a>(
    host: &'a dyn HookHost,
    hooks: Option<Arc<dyn HookRuntime>>,
    session_id: &'a str,
    timeout: Option<Duration>,
) -> BoxFuture<'a, Result<Option<String>, YourAiError>> {
    Box::pin(async move {
        let hook_result = bounded(
            timeout.map(|t| t / 4),
            host.dispatch_hook(HookEvent::SessionEnd {
                reason: "shutdown".into(),
            }),
        )
        .await
        .map_err(|_| {
            ErrorKind::Provider {
                name: "hook",
                message: "SessionEnd cleanup deadline exceeded".into(),
            }
            .into()
        })
        .and_then(|r| r);
        if let Some(hooks) = hooks {
            hooks.shutdown_session(session_id).await?;
        }
        Ok(match hook_result {
            Err(e) => Some(e.to_string()),
            Ok(result) => {
                let errors: Vec<_> = result
                    .common
                    .messages
                    .iter()
                    .filter(|m| matches!(m.kind, HookMessageKind::NonBlockingError))
                    .map(|m| m.content.clone())
                    .collect();
                (!errors.is_empty()).then(|| errors.join("\n"))
            }
        })
    })
}

/// 固定公共操作：回合完成通知（不消费阻断语义）。
///
/// 仅当回合内产生了新的历史条目时派发；所有可见输出统一以警告通知转发。
pub fn turn_completed<'a>(
    host: &'a dyn HookHost,
    turn_id: &'a TurnId,
    after_seq: i64,
    through_seq: i64,
    notify: &'a (dyn Fn(&str) + Send + Sync),
) -> BoxFuture<'a, ()> {
    Box::pin(async move {
        if through_seq <= after_seq {
            return;
        }
        let dispatched = host
            .dispatch_hook(HookEvent::TurnCompleted {
                turn_id: turn_id.to_string(),
                after_seq,
                through_seq,
            })
            .await;
        match dispatched {
            Ok(result) => {
                for message in result
                    .visible_messages()
                    .map(|m| m.content.clone())
                    .chain(result.common.system_messages.iter().cloned())
                    .chain(
                        result
                            .common
                            .blocking_errors
                            .iter()
                            .map(|e| e.message.clone()),
                    )
                {
                    notify(&message);
                }
            }
            Err(e) => notify(&format!("TurnCompleted hook failed: {e}")),
        }
    })
}

async fn bounded<T>(
    timeout: Option<Duration>,
    fut: impl Future<Output = T>,
) -> Result<T, tokio::time::error::Elapsed> {
    match timeout {
        Some(limit) => tokio::time::timeout(limit, fut).await,
        None => Ok(fut.await),
    }
}
