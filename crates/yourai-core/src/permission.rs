//! One permission lifecycle; no executor or per-tool branches.
use crate::hooks::{PermissionRequestBehavior, PermissionRequestDecision};
use crate::interaction::validate_schema;
use crate::prelude::*;
use serde_json::{json, Value};
async fn check_policy(
    turn: &mut crate::execution::Turn<'_>,
    handler: &Tool,
    input: &Value,
) -> Result<ApprovalDecision, YourAiError> {
    let Some(security) = turn.tc.snap.security.clone() else {
        return Ok(ApprovalDecision::Allow);
    };
    let context = handler.security_context(
        input,
        turn.tc
            .info
            .options
            .session
            .as_ref()
            .map(|s| s.cwd.as_path()),
    );
    turn.wait_operation(
        security.check_tool_call(&context),
        turn.op_timeout(),
        "security",
    )
    .await
}
pub async fn authorize(
    turn: &mut crate::execution::Turn<'_>,
    call: &mut ToolCall,
    handler: &Tool,
    hook_permission: HookPermission,
) -> Result<(), YourAiError> {
    turn.ensure_active()?;
    if let Some(schema) = &handler.definition().schema {
        validate_schema(schema, &call.fn_arguments)?;
    }
    if !matches!(hook_permission, HookPermission::Deny { .. })
        && turn
            .tc
            .snap
            .security
            .as_ref()
            .is_some_and(|s| s.bypass_approvals())
    {
        return turn.tc.check_control();
    }
    for attempt in 0..=turn.config.max_permission_rechecks {
        let security = check_policy(turn, handler, &call.fn_arguments).await?;
        let deny = match &hook_permission {
            HookPermission::Deny { reason } => Some(reason.clone()),
            _ if matches!(security, ApprovalDecision::Deny) => {
                Some("security policy denied tool".into())
            }
            _ => None,
        };
        let mut denied = deny;
        if denied.is_none() {
            let ask = matches!(hook_permission, HookPermission::Ask { .. })
                || matches!(security, ApprovalDecision::Ask);
            if !ask {
                return Ok(());
            }
            let result = turn
                .hook(HookEvent::PermissionRequest {
                    tool_name: call.fn_name.clone(),
                    tool_input: call.fn_arguments.clone(),
                    permission_suggestions: None,
                })
                .await?;
            turn.apply_common(&result)?;
            if !result.common.blocking_errors.is_empty() {
                denied = Some(result.blocking_messages().join("\n"));
            } else {
                let decision = match result.outcome {
                    HookPointOutcome::PermissionRequest(o) => o.decision,
                    _ => {
                        return Err(
                            ErrorKind::Loop("invalid PermissionRequest outcome".into()).into()
                        )
                    }
                };
                let (decision, remember) = match decision {
                    Some(decision) => (decision, false),
                    None => {
                        let reason = match &hook_permission {
                            HookPermission::Ask { reason } => reason.as_str(),
                            _ => "Tool permission",
                        };
                        let payload = json!({
                            "kind": "permission",
                            "reason": reason,
                            "call_id": call.call_id,
                            "tool_name": call.fn_name,
                            "input": call.fn_arguments,
                        });
                        let timeout = turn
                            .tc
                            .info
                            .options
                            .limits
                            .approval_timeout
                            .or(turn.config.approval_timeout);
                        let reply = turn
                            .ask(uuid::Uuid::new_v4().to_string(), payload, timeout)
                            .await;
                        match reply {
                            Ok(reply) => parse_decision(reply)?,
                            Err(e @ YourAiError::Aborted(_)) => return Err(e),
                            Err(e) => (
                                PermissionRequestDecision {
                                    behavior: PermissionRequestBehavior::Deny,
                                    updated_input: None,
                                    updated_permissions: vec![],
                                    message: Some(e.to_string()),
                                    interrupt: false,
                                },
                                false,
                            ),
                        }
                    }
                };
                if decision.interrupt {
                    return Err(AbortReason::HookStopped(
                        decision
                            .message
                            .unwrap_or_else(|| "permission hook interrupted".into()),
                    )
                    .into());
                }
                if decision.behavior == PermissionRequestBehavior::Allow {
                    if !decision.updated_permissions.is_empty() {
                        let policy = turn.tc.snap.security.clone().ok_or_else(|| {
                            ErrorKind::Config("permission updates require SecurityProvider".into())
                        })?;
                        turn.wait_operation(
                            policy.update_permissions(&decision.updated_permissions),
                            turn.op_timeout(),
                            "security",
                        )
                        .await?;
                    }
                    let input = decision
                        .updated_input
                        .unwrap_or_else(|| call.fn_arguments.clone());
                    if let Some(schema) = &handler.definition().schema {
                        validate_schema(schema, &input)?;
                    }
                    // Explicit approval covers exactly this final input; still recheck hard policy.
                    call.fn_arguments = input;
                    if matches!(
                        check_policy(turn, handler, &call.fn_arguments).await?,
                        ApprovalDecision::Deny
                    ) {
                        denied = Some("security policy denied approved input".into());
                    } else {
                        if remember {
                            let policy = turn.tc.snap.security.clone().ok_or_else(|| {
                                ErrorKind::Config(
                                    "session approvals require SecurityProvider".into(),
                                )
                            })?;
                            turn.wait_operation(
                                policy.remember_tool_approval(&call.fn_name),
                                turn.op_timeout(),
                                "security",
                            )
                            .await?;
                        }
                        return Ok(());
                    }
                } else {
                    denied = Some(
                        decision
                            .message
                            .unwrap_or_else(|| "permission denied".into()),
                    );
                }
            }
        }
        let reason = denied.unwrap_or_else(|| "permission denied".into());
        let result = turn
            .hook(HookEvent::PermissionDenied {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_use_id: call.call_id.clone(),
                reason: reason.clone(),
            })
            .await?;
        turn.apply_common(&result)?;
        let retry = matches!(result.outcome, HookPointOutcome::PermissionDenied(ref o) if o.retry);
        if retry && attempt < turn.config.max_permission_rechecks {
            continue;
        }
        return Err(ErrorKind::Tool {
            name: call.fn_name.clone(),
            message: reason,
        }
        .into());
    }
    unreachable!("inclusive permission loop always returns")
}
fn parse_decision(value: Value) -> Result<(PermissionRequestDecision, bool), YourAiError> {
    // UI replies cannot rewrite arguments or arbitrary policy. Session scope
    // can only remember the tool named by the immutable approval request.
    let behavior = match value.get("behavior").and_then(Value::as_str) {
        Some("allow") => PermissionRequestBehavior::Allow,
        Some("deny") => PermissionRequestBehavior::Deny,
        _ => {
            return Err(ErrorKind::Tool {
                name: "approval".into(),
                message: "expected behavior=allow|deny".into(),
            }
            .into())
        }
    };
    let remember = match value.get("scope") {
        None => false,
        Some(scope) if scope == "session" && behavior == PermissionRequestBehavior::Allow => true,
        _ => return Err(ErrorKind::Config("invalid approval scope".into()).into()),
    };
    Ok((
        PermissionRequestDecision {
            behavior,
            updated_input: None,
            updated_permissions: vec![],
            message: None,
            interrupt: false,
        },
        remember,
    ))
}

#[cfg(test)]
mod tests {
    use super::parse_decision;
    use serde_json::json;

    #[test]
    fn reply_scope_is_limited_to_allowing_the_displayed_tool_for_this_session() {
        assert!(!parse_decision(json!({"behavior":"allow"})).unwrap().1);
        assert!(
            parse_decision(json!({"behavior":"allow","scope":"session"}))
                .unwrap()
                .1
        );
        for value in [
            json!({"behavior":"deny","scope":"session"}),
            json!({"behavior":"allow","scope":"user"}),
        ] {
            assert!(parse_decision(value).is_err());
        }
        let (decision, _) = parse_decision(json!({"behavior":"allow","scope":"session","updated_input":{"x":1},"updated_permissions":[{}]})).unwrap();
        assert!(decision.updated_input.is_none());
        assert!(decision.updated_permissions.is_empty());
    }
}
