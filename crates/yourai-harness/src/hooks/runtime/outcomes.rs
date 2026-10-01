//! Hook wire interpretation and aggregation; no scheduling or I/O.
use super::*;

// ── Output parsing ──────────────────────────────────────────────────

pub(super) fn parse_handler_output(
    output: &HookOutput,
    expected_event: &str,
) -> Result<Contribution, YourAiError> {
    let mut contrib = Contribution::default();

    match output {
        HookOutput::Parsed(json_value) => {
            let json_output: HookJsonOutput = parse_json_value(json_value)?;
            apply_json_output(&json_output, expected_event, &mut contrib)?;
        }
        HookOutput::Command {
            stdout,
            stderr,
            exit_code,
        } => match exit_code {
            0 => {
                let trimmed = stdout.trim();
                if trimmed.is_empty() {
                    // 空输出 = 空成功
                } else if trimmed.starts_with('{') {
                    match crate::hooks::wire_output::parse_hook_json(stdout) {
                        Ok(json_output) => {
                            apply_json_output(&json_output, expected_event, &mut contrib)?;
                        }
                        Err(e) => {
                            let kind = match e.classify() {
                                serde_json::error::Category::Syntax
                                | serde_json::error::Category::Eof => HookMessageKind::Success,
                                serde_json::error::Category::Data
                                | serde_json::error::Category::Io => {
                                    HookMessageKind::NonBlockingError
                                }
                            };
                            let content = if kind == HookMessageKind::Success {
                                trimmed.to_string()
                            } else {
                                format!("Hook JSON output validation failed: {e}")
                            };
                            contrib.message = Some((kind, content));
                        }
                    }
                } else {
                    contrib.message = Some((HookMessageKind::Success, trimmed.to_string()));
                }
            }
            2 => {
                contrib.blocking_error = Some(if stderr.trim().is_empty() {
                    "blocked by hook (exit 2)".to_string()
                } else {
                    stderr.trim().to_string()
                });
            }
            _ => {
                contrib.message = Some((
                    HookMessageKind::NonBlockingError,
                    format!(
                        "Hook failed with non-blocking status {exit_code}: {}",
                        if stderr.trim().is_empty() {
                            "No stderr output"
                        } else {
                            stderr.trim()
                        }
                    ),
                ));
            }
        },
        HookOutput::Http { body, status: _ } => {
            let trimmed = body.trim();
            if trimmed.is_empty() {
                // 空 body = 空成功
            } else if trimmed.starts_with('{') {
                match crate::hooks::wire_output::parse_hook_json(body) {
                    Ok(json_output) => {
                        apply_json_output(&json_output, expected_event, &mut contrib)?;
                    }
                    Err(e) => {
                        contrib.message = Some((
                            HookMessageKind::NonBlockingError,
                            format!("HTTP Hook JSON output validation failed: {e}"),
                        ));
                    }
                }
            } else {
                contrib.message = Some((
                    HookMessageKind::NonBlockingError,
                    format!("HTTP Hook must return JSON, got: {trimmed}"),
                ));
            }
        }
        _ => {}
    }

    Ok(contrib)
}

fn parse_json_value(value: &Value) -> Result<HookJsonOutput, YourAiError> {
    crate::hooks::wire_output::parse_hook_json(&value.to_string()).map_err(|error| {
        ErrorKind::Provider {
            name: "hook",
            message: format!("parsed hook output validation failed: {error}"),
        }
        .into()
    })
}

fn apply_json_output(
    json_output: &HookJsonOutput,
    expected_event: &str,
    contrib: &mut Contribution,
) -> Result<(), YourAiError> {
    match json_output {
        HookJsonOutput::Async(_) => {}
        HookJsonOutput::Sync(s) => {
            apply_sync_output(s, expected_event, contrib)?;
        }
    }
    Ok(())
}

fn apply_sync_output(
    s: &SyncOutput,
    expected_event: &str,
    contrib: &mut Contribution,
) -> Result<(), YourAiError> {
    contrib.suppress_output = s.suppress_output.unwrap_or(false);

    if s.continue_field == Some(false) {
        contrib.prevent_continuation = true;
        contrib.stop_reason = s.stop_reason.clone();
    }

    if let Some(msg) = &s.system_message {
        contrib.system_message = Some(msg.clone());
    }

    if let Some(decision) = &s.decision {
        match decision {
            LegacyDecision::Approve => {
                contrib.permission = Some(HookPermission::Allow {
                    reason: s.reason.clone(),
                });
            }
            LegacyDecision::Block => {
                let reason = s
                    .reason
                    .clone()
                    .unwrap_or_else(|| "blocked by hook".to_string());
                contrib.permission = Some(HookPermission::Deny {
                    reason: reason.clone(),
                });
                contrib.blocking_error = Some(reason);
            }
        }
    }

    if let Some(hso) = &s.hook_specific_output {
        let hso_event = hso.event_name();
        if hso_event != expected_event {
            return Err(ErrorKind::Provider {
                name: "hook",
                message: format!(
                    "hookEventName mismatch: expected {expected_event}, got {hso_event}"
                ),
            }
            .into());
        }
        apply_hook_specific_output(hso, contrib);
    }

    Ok(())
}

fn apply_hook_specific_output(hso: &HookSpecificOutput, contrib: &mut Contribution) {
    match hso {
        HookSpecificOutput::PreToolUse {
            permission_decision,
            permission_decision_reason,
            updated_input,
            additional_context,
        } => {
            if let Some(pd) = permission_decision {
                match pd {
                    PermissionBehavior::Allow => {
                        contrib.permission = Some(HookPermission::Allow {
                            reason: permission_decision_reason.clone(),
                        });
                    }
                    PermissionBehavior::Deny => {
                        let reason = permission_decision_reason
                            .clone()
                            .unwrap_or_else(|| "denied by hook".to_string());
                        contrib.permission = Some(HookPermission::Deny {
                            reason: reason.clone(),
                        });
                        contrib.blocking_error = Some(reason);
                    }
                    PermissionBehavior::Ask => {
                        contrib.permission = Some(HookPermission::Ask {
                            reason: permission_decision_reason
                                .clone()
                                .unwrap_or_else(|| "hook requests user confirmation".to_string()),
                        });
                    }
                }
            }
            if let Some(ui) = updated_input {
                contrib.updated_input = Some(Value::Object(ui.clone()));
            }
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
        }
        HookSpecificOutput::UserPromptSubmit { additional_context }
        | HookSpecificOutput::Setup { additional_context }
        | HookSpecificOutput::SubagentStart { additional_context }
        | HookSpecificOutput::PostToolUseFailure { additional_context }
        | HookSpecificOutput::Notification { additional_context } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
        }
        HookSpecificOutput::SessionStart {
            additional_context,
            initial_user_message,
            watch_paths,
        } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
            if let Some(msg) = initial_user_message {
                contrib.initial_user_message = Some(msg.clone());
            }
            if let Some(wp) = watch_paths {
                contrib.watch_paths = wp.clone();
            }
        }
        HookSpecificOutput::PostToolUse {
            additional_context,
            updated_mcp_tool_output,
        } => {
            if let Some(ac) = additional_context {
                contrib.additional_context = Some(ac.clone());
            }
            if let Some(umto) = updated_mcp_tool_output {
                contrib.updated_mcp_tool_output = Some(umto.clone());
            }
        }
        HookSpecificOutput::PermissionDenied { retry } => {
            contrib.retry = *retry;
        }
        HookSpecificOutput::PermissionRequest { decision } => {
            let (behavior, updated_input, updated_permissions, message, interrupt) = match decision
            {
                crate::hooks::wire_output::PermissionRequestDecision::Allow {
                    updated_input,
                    updated_permissions,
                } => (
                    PermissionRequestBehavior::Allow,
                    updated_input.clone().map(Value::Object),
                    updated_permissions
                        .as_ref()
                        .map(|v| {
                            v.iter()
                                .map(|p| serde_json::to_value(p).unwrap_or(Value::Null))
                                .collect()
                        })
                        .unwrap_or_default(),
                    None,
                    false,
                ),
                crate::hooks::wire_output::PermissionRequestDecision::Deny {
                    message,
                    interrupt,
                } => (
                    PermissionRequestBehavior::Deny,
                    None,
                    vec![],
                    message.clone(),
                    interrupt.unwrap_or(false),
                ),
            };
            contrib.permission_request_decision = Some(PermissionRequestDecision {
                behavior,
                updated_input,
                updated_permissions,
                message,
                interrupt,
            });
        }
        HookSpecificOutput::CwdChanged { watch_paths }
        | HookSpecificOutput::FileChanged { watch_paths } => {
            if let Some(wp) = watch_paths {
                contrib.watch_paths = wp.clone();
            }
        }
        HookSpecificOutput::Elicitation { action, content }
        | HookSpecificOutput::ElicitationResult { action, content } => {
            contrib.elicitation_action = *action;
            contrib.elicitation_content = content.clone().map(Value::Object);
            if *action == Some(ElicitationAction::Decline) {
                contrib.blocking_error = Some("elicitation declined by hook".to_string());
            }
        }
        HookSpecificOutput::WorktreeCreate { worktree_path } => {
            contrib.worktree_path = Some(worktree_path.clone());
        }
    }
}

// ── Aggregation ─────────────────────────────────────────────────────

pub(super) fn aggregate(event_name: &str, contributions: Vec<Contribution>) -> HookPointOutcome {
    match event_name {
        "PreToolUse" => HookPointOutcome::PreToolUse(aggregate_pre_tool_use(contributions)),
        "PostToolUse" => HookPointOutcome::PostToolUse(aggregate_post_tool_use(contributions)),
        "UserPromptSubmit" => {
            HookPointOutcome::UserPromptSubmit(aggregate_user_prompt_submit(contributions))
        }
        "SessionStart" => HookPointOutcome::SessionStart(aggregate_session_start(contributions)),
        "PermissionDenied" => {
            HookPointOutcome::PermissionDenied(aggregate_permission_denied(contributions))
        }
        "PermissionRequest" => {
            HookPointOutcome::PermissionRequest(aggregate_permission_request(contributions))
        }
        "CwdChanged" | "FileChanged" => {
            let wp = aggregate_watch_paths(contributions);
            if event_name == "CwdChanged" {
                HookPointOutcome::CwdChanged(wp)
            } else {
                HookPointOutcome::FileChanged(wp)
            }
        }
        "Elicitation" => HookPointOutcome::Elicitation(aggregate_elicitation(contributions)),
        "ElicitationResult" => {
            HookPointOutcome::ElicitationResult(aggregate_elicitation(contributions))
        }
        "WorktreeCreate" => {
            HookPointOutcome::WorktreeCreate(aggregate_worktree_create(contributions))
        }
        _ => HookPointOutcome::Generic(aggregate_generic(contributions)),
    }
}

pub(super) fn aggregate_common(contributions: &[Contribution]) -> HookCommonOutcome {
    let prevent_continuation = contributions.iter().any(|c| c.prevent_continuation);
    let stop_reason = contributions
        .iter()
        .rev()
        .find_map(|c| c.stop_reason.clone());
    let system_messages = contributions
        .iter()
        .filter_map(|c| c.system_message.clone())
        .collect();
    let messages = contributions
        .iter()
        .filter_map(|c| {
            c.message.clone().map(|(kind, content)| HookMessage {
                hook_id: c.hook_id.clone(),
                kind,
                content,
            })
        })
        .collect();
    let blocking_errors = contributions
        .iter()
        .filter_map(|c| {
            c.blocking_error.clone().map(|message| HookBlockingError {
                hook_id: c.hook_id.clone(),
                message,
            })
        })
        .collect();

    HookCommonOutcome {
        prevent_continuation,
        stop_reason,
        system_messages,
        messages,
        blocking_errors,
    }
}

fn aggregate_contexts(contributions: &[Contribution]) -> Vec<String> {
    contributions
        .iter()
        .filter_map(|c| c.additional_context.clone())
        .collect()
}

pub(super) fn aggregate_pre_tool_use(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PreToolUseOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let mut permission = HookPermission::Pass;
    let mut updated_input = None;

    for c in &contributions {
        if let Some(p) = &c.permission {
            permission = permission.merge(p.clone());
        }
        if let Some(ui) = &c.updated_input {
            if !matches!(&c.permission, Some(HookPermission::Deny { .. })) {
                updated_input = Some(ui.clone());
            }
        }
    }

    yourai_core::hooks::PreToolUseOutcome {
        permission,
        updated_input,
        additional_contexts,
    }
}

fn aggregate_post_tool_use(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PostToolUseOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let updated_mcp_tool_output = contributions
        .iter()
        .rev()
        .find_map(|c| c.updated_mcp_tool_output.clone());

    yourai_core::hooks::PostToolUseOutcome {
        additional_contexts,
        updated_mcp_tool_output,
    }
}

fn aggregate_user_prompt_submit(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::UserPromptSubmitOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    yourai_core::hooks::UserPromptSubmitOutcome {
        additional_contexts,
    }
}

fn aggregate_session_start(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::SessionStartOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    let initial_user_message = contributions
        .iter()
        .rev()
        .find_map(|c| c.initial_user_message.clone());
    let watch_paths: Vec<String> = contributions
        .iter()
        .flat_map(|c| c.watch_paths.clone())
        .collect();

    yourai_core::hooks::SessionStartOutcome {
        additional_contexts,
        initial_user_message,
        watch_paths,
    }
}

fn aggregate_permission_denied(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PermissionDeniedOutcome {
    let retry = contributions.iter().any(|c| c.retry == Some(true));
    yourai_core::hooks::PermissionDeniedOutcome { retry }
}

fn aggregate_permission_request(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::PermissionRequestOutcome {
    let decision = contributions
        .iter()
        .rev()
        .find_map(|c| c.permission_request_decision.clone());
    yourai_core::hooks::PermissionRequestOutcome { decision }
}

fn aggregate_watch_paths(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::WatchPathsOutcome {
    let watch_paths: Vec<String> = contributions
        .iter()
        .flat_map(|c| c.watch_paths.clone())
        .collect();
    yourai_core::hooks::WatchPathsOutcome { watch_paths }
}

fn aggregate_elicitation(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::ElicitationOutcome {
    let action = contributions
        .iter()
        .rev()
        .find_map(|c| c.elicitation_action);
    let content = contributions
        .iter()
        .rev()
        .find_map(|c| c.elicitation_content.clone());
    yourai_core::hooks::ElicitationOutcome {
        action: action.map(|value| {
            match value {
                ElicitationAction::Accept => "accept",
                ElicitationAction::Decline => "decline",
                ElicitationAction::Cancel => "cancel",
            }
            .to_string()
        }),
        content,
    }
}

fn aggregate_worktree_create(
    contributions: Vec<Contribution>,
) -> yourai_core::hooks::WorktreeCreateOutcome {
    let worktree_path = contributions
        .iter()
        .rev()
        .find_map(|c| c.worktree_path.clone());
    yourai_core::hooks::WorktreeCreateOutcome { worktree_path }
}

fn aggregate_generic(contributions: Vec<Contribution>) -> yourai_core::hooks::GenericOutcome {
    let additional_contexts = aggregate_contexts(&contributions);
    yourai_core::hooks::GenericOutcome {
        additional_contexts,
    }
}
