use super::{
    hooks,
    interaction::{validate_schema, Bridge},
    State,
};
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Arc};
use tokio::sync::mpsc;
use yourai_core::{
    hooks::{PermissionRequestBehavior, PermissionRequestDecision},
    prelude::*,
};

impl State<'_> {
    pub(crate) async fn execute_tool(&mut self, mut call: ToolCall) -> Result<(), YourAiError> {
        self.send(Out::ToolStarted {
            id: call.call_id.clone(),
            name: call.fn_name.clone(),
            input: call.fn_arguments.clone(),
        })?;
        let result = self.tool_body(&mut call).await;
        let aborted = result
            .as_ref()
            .err()
            .is_some_and(|e| matches!(e, YourAiError::Aborted(_)));
        let (mut output, is_error) = match &result {
            Ok(value) => (value.clone(), false),
            Err(error) => (json!({"error":error.to_string()}), true),
        };
        self.tool_completion = Some((call.clone(), output.clone(), is_error));
        let event = match &result {
            Ok(value) => HookEvent::PostToolUse {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_response: value.clone(),
                tool_use_id: call.call_id.clone(),
            },
            Err(error) => HookEvent::PostToolUseFailure {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_use_id: call.call_id.clone(),
                error: error.to_string(),
                is_interrupt: Some(aborted),
            },
        };
        // On cancellation, invoke failure reporting with a separate bounded cleanup wait.
        let post = if aborted {
            if let Some(runtime) = &self.tc.snap.hooks {
                let invocation = self.invocation(event);
                tokio::time::timeout(self.config.cleanup_timeout, runtime.dispatch(&invocation))
                    .await
                    .ok()
                    .and_then(Result::ok)
            } else {
                None
            }
        } else {
            Some(self.hook(event).await?)
        };
        let mut post_error = None;
        let mut contexts = vec![];
        if let Some(post) = post {
            post_error = self.apply_common(&post).err();
            contexts = hooks::additional(&post);
            contexts.extend(hooks::feedback(&post));
            if let HookPointOutcome::PostToolUse(o) = post.outcome {
                if call.fn_name.starts_with("mcp__") {
                    if let Some(value) = o.updated_mcp_tool_output {
                        output = value;
                    }
                }
            }
        }
        self.tool_completion = Some((call.clone(), output.clone(), is_error));
        if aborted {
            return result.map(|_| ());
        } // cleanup commits the result with fresh time allowance
        let history = self.history.clone();
        self.wait(
            history.append(vec![result_record(&call, &output, is_error)]),
            self.op_timeout(),
            "history",
        )
        .await?;
        self.unresolved.pop_front();
        self.tool_completion = None;
        self.send(Out::ToolDone {
            id: call.call_id,
            name: call.fn_name,
            output,
            is_error,
        })?;
        if let Some(error) = post_error {
            return Err(error);
        }
        self.deferred_context.extend(contexts);
        Ok(())
    }

    async fn tool_body(&mut self, call: &mut ToolCall) -> Result<Value, YourAiError> {
        let handler = self
            .bound_tools
            .get(&call.fn_name)
            .cloned()
            .ok_or_else(|| ErrorKind::Tool {
                name: call.fn_name.clone(),
                message: "tool was not offered in this model request".into(),
            })?;
        let pre = self
            .hook(HookEvent::PreToolUse {
                tool_name: call.fn_name.clone(),
                tool_input: call.fn_arguments.clone(),
                tool_use_id: call.call_id.clone(),
            })
            .await?;
        self.apply_common(&pre)?;
        self.deferred_context.extend(hooks::additional(&pre));
        let blocking = hooks::feedback(&pre);
        let outcome = match pre.outcome {
            HookPointOutcome::PreToolUse(o) => o,
            _ => return Err(ErrorKind::Loop("invalid PreToolUse outcome".into()).into()),
        };
        if let Some(input) = outcome.updated_input {
            call.fn_arguments = input;
        }
        let permission = if !blocking.is_empty() {
            HookPermission::Deny {
                reason: blocking.join("\n"),
            }
        } else {
            outcome.permission
        };
        let definition = handler.definition();
        if let Some(schema) = &definition.schema {
            validate_schema(schema, &call.fn_arguments)?;
        }
        self.approve(call, &handler, permission).await?;
        self.tc.check_control()?;
        let limit = self
            .tc
            .info
            .options
            .limits
            .max_tool_calls
            .unwrap_or(self.config.max_tool_calls)
            .min(self.config.max_tool_calls);
        if self.tool_calls >= limit {
            return Err(AbortReason::LimitReached(TurnLimit::ToolCalls).into());
        }
        self.tool_calls += 1;
        let (tx, mut rx) = mpsc::unbounded_channel();
        let bridge = Bridge { tx };
        let cancel = self.tc.cancel.child_token();
        let _guard = cancel.clone().drop_guard();
        let tool_context = ToolContext {
            call_id: call.call_id.clone(),
            emit: self.tc.outbox,
            cancel: &cancel,
            security: self.tc.snap.security.clone(),
            sandbox: self.tc.snap.sandbox.clone(),
            interaction: Some(&bridge),
        };
        let future = handler.execute(tool_context, call.fn_arguments.clone());
        tokio::pin!(future);
        let deadline = self.deadline(Some(
            self.tc
                .info
                .options
                .limits
                .tool_timeout
                .unwrap_or(self.config.operation_timeout),
        ));
        let mut request_ids = HashSet::new();
        loop {
            tokio::select! {
                biased;
                _ = self.tc.cancel.cancelled() => return Err(AbortReason::Cancelled.into()),
                _ = self.tc.outbox.closed() => return Err(AbortReason::Disconnected.into()),
                _ = tokio::time::sleep_until(deadline.into()) => return Err(self.timeout_error("tool")),
                result = &mut future => return result,
                Some(mut pending) = rx.recv() => {
                    if pending.reply.is_closed() { continue; }
                    if pending.request.call_id != call.call_id || !request_ids.insert(pending.request.id.clone()) {
                        let _ = pending.reply.send(Err(ErrorKind::Config("invalid or duplicate interaction identity".into()).into()));
                        continue;
                    }
                    pending.request.deadline = Some(pending.request.deadline.unwrap_or(deadline).min(deadline));
                    let interaction_deadline = pending.request.deadline.unwrap();
                    let response = tokio::select! {
                        biased;
                        result = &mut future => return result,
                        _ = pending.reply.closed() => continue,
                        result = tokio::time::timeout_at(interaction_deadline.into(), self.service_interaction(pending.request)) => result,
                    };
                    let response = response.unwrap_or_else(|_| Err(self.timeout_error("interaction")));
                    let terminal = response.as_ref().err().is_some_and(|e| matches!(e, YourAiError::Aborted(_)));
                    if terminal { return Err(response.unwrap_err()); }
                    let _ = pending.reply.send(response);
                }
                input = self.tc.inbox.recv(), if !self.input_closed => match input {
                    Some(input) => self.route(input), None => self.input_closed = true,
                }
            }
        }
    }
    async fn security(
        &mut self,
        handler: &Arc<dyn ToolHandler>,
        input: &Value,
    ) -> Result<ApprovalDecision, YourAiError> {
        let Some(security) = self.tc.snap.security.clone() else {
            return Ok(ApprovalDecision::Allow);
        };
        let context = handler.security_context(input);
        self.wait(
            security.check_tool_call(&context),
            self.op_timeout(),
            "security",
        )
        .await
    }
    async fn approve(
        &mut self,
        call: &mut ToolCall,
        handler: &Arc<dyn ToolHandler>,
        hook_permission: HookPermission,
    ) -> Result<(), YourAiError> {
        for attempt in 0..=self.config.max_permission_rechecks {
            let security = self.security(handler, &call.fn_arguments).await?;
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
                let result = self
                    .hook(HookEvent::PermissionRequest {
                        tool_name: call.fn_name.clone(),
                        tool_input: call.fn_arguments.clone(),
                        permission_suggestions: None,
                    })
                    .await?;
                self.apply_common(&result)?;
                if !result.common.blocking_errors.is_empty() {
                    denied = Some(hooks::feedback(&result).join("\n"));
                } else {
                    let decision = match result.outcome {
                        HookPointOutcome::PermissionRequest(o) => o.decision,
                        _ => {
                            return Err(
                                ErrorKind::Loop("invalid PermissionRequest outcome".into()).into()
                            )
                        }
                    };
                    let decision = match decision {
                        Some(decision) => decision,
                        None => {
                            let reply = self.ask(uuid::Uuid::new_v4().to_string(), json!({"kind":"permission", "call_id":call.call_id, "tool_name":call.fn_name, "input":call.fn_arguments}), self.tc.info.options.limits.approval_timeout.unwrap_or(self.config.approval_timeout)).await;
                            match reply {
                                Ok(reply) => parse_decision(reply)?,
                                Err(e @ YourAiError::Aborted(_)) => return Err(e),
                                Err(e) => PermissionRequestDecision {
                                    behavior: PermissionRequestBehavior::Deny,
                                    updated_input: None,
                                    updated_permissions: vec![],
                                    message: Some(e.to_string()),
                                    interrupt: false,
                                },
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
                            let policy = self.tc.snap.security.clone().ok_or_else(|| {
                                ErrorKind::Config(
                                    "permission updates require SecurityProvider".into(),
                                )
                            })?;
                            self.wait(
                                policy.update_permissions(&decision.updated_permissions),
                                self.op_timeout(),
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
                            self.security(handler, &call.fn_arguments).await?,
                            ApprovalDecision::Deny
                        ) {
                            denied = Some("security policy denied approved input".into());
                        } else {
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
            let result = self
                .hook(HookEvent::PermissionDenied {
                    tool_name: call.fn_name.clone(),
                    tool_input: call.fn_arguments.clone(),
                    tool_use_id: call.call_id.clone(),
                    reason: reason.clone(),
                })
                .await?;
            self.apply_common(&result)?;
            let retry =
                matches!(result.outcome, HookPointOutcome::PermissionDenied(ref o) if o.retry);
            if retry && attempt < self.config.max_permission_rechecks {
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
}
fn parse_decision(value: Value) -> Result<PermissionRequestDecision, YourAiError> {
    // UI replies cannot rewrite arguments or policy. The displayed scope is immutable.
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
    Ok(PermissionRequestDecision {
        behavior,
        updated_input: None,
        updated_permissions: vec![],
        message: None,
        interrupt: false,
    })
}

pub(super) fn result_record(call: &ToolCall, output: &Value, _is_error: bool) -> StoredMessage {
    StoredMessage::new(ToolResponse::from_tool_call(call, output.to_string()).into())
}
