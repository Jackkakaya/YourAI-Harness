//! Claude 兼容的 wire 输出类型（反序列化）。
//!
//! 解析 hook 返回的 JSON（command stdout 或 HTTP response body）。
//! 输出字段采用 camelCase，与 Claude Code hook 协议完全一致。

use serde::{Deserialize, Serialize};
use serde_json::Value;

// ── Top-level output ────────────────────────────────────────────────

/// Hook JSON 输出：async 或 sync 两个分支。
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum HookJsonOutput {
    Async(AsyncOutput),
    Sync(SyncOutput),
}

/// 异步输出：`{ "async": true, "asyncTimeout"?: number }`
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct AsyncOutput {
    #[serde(rename = "async")]
    pub is_async: bool,
    #[serde(default, rename = "asyncTimeout")]
    pub async_timeout: Option<u64>,
}

/// 同步输出：公共字段 + 可选的 `hookSpecificOutput`。
#[derive(Debug, Clone, Deserialize, Default, Serialize)]
pub struct SyncOutput {
    #[serde(default, rename = "continue")]
    pub continue_field: Option<bool>,
    #[serde(default, rename = "suppressOutput")]
    pub suppress_output: Option<bool>,
    #[serde(default, rename = "stopReason")]
    pub stop_reason: Option<String>,
    #[serde(default)]
    pub decision: Option<LegacyDecision>,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default, rename = "systemMessage")]
    pub system_message: Option<String>,
    #[serde(default, rename = "hookSpecificOutput")]
    pub hook_specific_output: Option<HookSpecificOutput>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum LegacyDecision {
    Approve,
    Block,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum PermissionBehavior {
    Allow,
    Deny,
    Ask,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum ElicitationAction {
    Accept,
    Decline,
    Cancel,
}

// ── hookSpecificOutput discriminated union ──────────────────────────

/// 事件专属输出（discriminated union on `hookEventName`）。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "hookEventName")]
pub enum HookSpecificOutput {
    #[serde(rename = "PreToolUse")]
    PreToolUse {
        #[serde(default, rename = "permissionDecision")]
        permission_decision: Option<PermissionBehavior>,
        #[serde(default, rename = "permissionDecisionReason")]
        permission_decision_reason: Option<String>,
        #[serde(default, rename = "updatedInput")]
        updated_input: Option<serde_json::Map<String, Value>>,
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "UserPromptSubmit")]
    UserPromptSubmit {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "SessionStart")]
    SessionStart {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
        #[serde(default, rename = "initialUserMessage")]
        initial_user_message: Option<String>,
        #[serde(default, rename = "watchPaths")]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "Setup")]
    Setup {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "SubagentStart")]
    SubagentStart {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "PostToolUse")]
    PostToolUse {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
        #[serde(default, rename = "updatedMCPToolOutput")]
        updated_mcp_tool_output: Option<Value>,
    },
    #[serde(rename = "PostToolUseFailure")]
    PostToolUseFailure {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "PermissionDenied")]
    PermissionDenied {
        #[serde(default)]
        retry: Option<bool>,
    },
    #[serde(rename = "Notification")]
    Notification {
        #[serde(default, rename = "additionalContext")]
        additional_context: Option<String>,
    },
    #[serde(rename = "PermissionRequest")]
    PermissionRequest { decision: PermissionRequestDecision },
    #[serde(rename = "CwdChanged")]
    CwdChanged {
        #[serde(default, rename = "watchPaths")]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "FileChanged")]
    FileChanged {
        #[serde(default, rename = "watchPaths")]
        watch_paths: Option<Vec<String>>,
    },
    #[serde(rename = "Elicitation")]
    Elicitation {
        #[serde(default)]
        action: Option<ElicitationAction>,
        #[serde(default)]
        content: Option<serde_json::Map<String, Value>>,
    },
    #[serde(rename = "ElicitationResult")]
    ElicitationResult {
        #[serde(default)]
        action: Option<ElicitationAction>,
        #[serde(default)]
        content: Option<serde_json::Map<String, Value>>,
    },
    #[serde(rename = "WorktreeCreate")]
    WorktreeCreate {
        #[serde(rename = "worktreePath")]
        worktree_path: String,
    },
}

impl HookSpecificOutput {
    /// 返回该输出的 `hookEventName` 值。
    pub fn event_name(&self) -> &'static str {
        match self {
            HookSpecificOutput::PreToolUse { .. } => "PreToolUse",
            HookSpecificOutput::UserPromptSubmit { .. } => "UserPromptSubmit",
            HookSpecificOutput::SessionStart { .. } => "SessionStart",
            HookSpecificOutput::Setup { .. } => "Setup",
            HookSpecificOutput::SubagentStart { .. } => "SubagentStart",
            HookSpecificOutput::PostToolUse { .. } => "PostToolUse",
            HookSpecificOutput::PostToolUseFailure { .. } => "PostToolUseFailure",
            HookSpecificOutput::PermissionDenied { .. } => "PermissionDenied",
            HookSpecificOutput::Notification { .. } => "Notification",
            HookSpecificOutput::PermissionRequest { .. } => "PermissionRequest",
            HookSpecificOutput::CwdChanged { .. } => "CwdChanged",
            HookSpecificOutput::FileChanged { .. } => "FileChanged",
            HookSpecificOutput::Elicitation { .. } => "Elicitation",
            HookSpecificOutput::ElicitationResult { .. } => "ElicitationResult",
            HookSpecificOutput::WorktreeCreate { .. } => "WorktreeCreate",
        }
    }
}

// ── PermissionRequest decision ──────────────────────────────────────

/// `PermissionRequest` 输出的嵌套 decision。
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "behavior")]
pub enum PermissionRequestDecision {
    #[serde(rename = "allow")]
    Allow {
        #[serde(default, rename = "updatedInput")]
        updated_input: Option<serde_json::Map<String, Value>>,
        #[serde(default, rename = "updatedPermissions")]
        updated_permissions: Option<Vec<PermissionUpdate>>,
    },
    #[serde(rename = "deny")]
    Deny {
        #[serde(default)]
        message: Option<String>,
        #[serde(default)]
        interrupt: Option<bool>,
    },
}

// ── PermissionUpdate (for updatedPermissions) ───────────────────────

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum PermissionUpdate {
    #[serde(rename = "addRules")]
    AddRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionUpdateDestination,
    },
    #[serde(rename = "replaceRules")]
    ReplaceRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionUpdateDestination,
    },
    #[serde(rename = "removeRules")]
    RemoveRules {
        rules: Vec<PermissionRuleValue>,
        behavior: PermissionBehavior,
        destination: PermissionUpdateDestination,
    },
    #[serde(rename = "setMode")]
    SetMode {
        mode: PermissionMode,
        destination: PermissionUpdateDestination,
    },
    #[serde(rename = "addDirectories")]
    AddDirectories {
        directories: Vec<String>,
        destination: PermissionUpdateDestination,
    },
    #[serde(rename = "removeDirectories")]
    RemoveDirectories {
        directories: Vec<String>,
        destination: PermissionUpdateDestination,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PermissionUpdateDestination {
    #[serde(rename = "userSettings")]
    UserSettings,
    #[serde(rename = "projectSettings")]
    ProjectSettings,
    #[serde(rename = "localSettings")]
    LocalSettings,
    #[serde(rename = "session")]
    Session,
    #[serde(rename = "cliArg")]
    CliArg,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
pub enum PermissionMode {
    #[serde(rename = "default")]
    Default,
    #[serde(rename = "acceptEdits")]
    AcceptEdits,
    #[serde(rename = "bypassPermissions")]
    BypassPermissions,
    #[serde(rename = "plan")]
    Plan,
    #[serde(rename = "dontAsk")]
    DontAsk,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct PermissionRuleValue {
    #[serde(rename = "toolName")]
    pub tool_name: String,
    #[serde(default, rename = "ruleContent")]
    pub rule_content: Option<String>,
}

// ── Parsing ─────────────────────────────────────────────────────────

/// 解析 hook stdout / HTTP body 为 `HookJsonOutput`。
///
/// 规则（与 Claude Code 一致）：
/// - 先尝试解析为 JSON Value
/// - 若 `"async" == true` → `Async`
/// - 否则 → `Sync`
/// - 空 body → `Sync({})` （空同步成功）
pub fn parse_hook_json(raw: &str) -> Result<HookJsonOutput, serde_json::Error> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return Ok(HookJsonOutput::Sync(SyncOutput::default()));
    }
    let value: Value = serde_json::from_str(trimmed)?;
    if value.get("async") == Some(&Value::Bool(true)) {
        let async_output: AsyncOutput = serde_json::from_value(value)?;
        Ok(HookJsonOutput::Async(async_output))
    } else {
        let sync_output: SyncOutput = serde_json::from_value(value)?;
        Ok(HookJsonOutput::Sync(sync_output))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_empty_as_sync_default() {
        let out = parse_hook_json("").unwrap();
        assert!(matches!(out, HookJsonOutput::Sync(_)));
    }

    #[test]
    fn parse_async() {
        let out = parse_hook_json(r#"{"async": true, "asyncTimeout": 60000}"#).unwrap();
        match out {
            HookJsonOutput::Async(a) => {
                assert!(a.is_async);
                assert_eq!(a.async_timeout, Some(60000));
            }
            _ => panic!("expected async"),
        }
    }

    #[test]
    fn parse_sync_pretooluse_deny() {
        let json = r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"deny","permissionDecisionReason":"blocked"}}"#;
        let out = parse_hook_json(json).unwrap();
        match out {
            HookJsonOutput::Sync(s) => {
                let hso = s.hook_specific_output.unwrap();
                match hso {
                    HookSpecificOutput::PreToolUse {
                        permission_decision,
                        permission_decision_reason,
                        ..
                    } => {
                        assert_eq!(permission_decision, Some(PermissionBehavior::Deny));
                        assert_eq!(permission_decision_reason.as_deref(), Some("blocked"));
                    }
                    _ => panic!("expected PreToolUse"),
                }
            }
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn parse_sync_continue_false() {
        let json = r#"{"continue": false, "stopReason": "policy"}"#;
        let out = parse_hook_json(json).unwrap();
        match out {
            HookJsonOutput::Sync(s) => {
                assert_eq!(s.continue_field, Some(false));
                assert_eq!(s.stop_reason.as_deref(), Some("policy"));
            }
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn parse_permission_request_allow() {
        let json = r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest","decision":{"behavior":"allow","updatedInput":{"x":1}}}}"#;
        let out = parse_hook_json(json).unwrap();
        match out {
            HookJsonOutput::Sync(s) => match s.hook_specific_output.unwrap() {
                HookSpecificOutput::PermissionRequest { decision } => match decision {
                    PermissionRequestDecision::Allow { updated_input, .. } => {
                        assert_eq!(
                            updated_input.map(Value::Object),
                            Some(serde_json::json!({"x": 1}))
                        );
                    }
                    _ => panic!("expected allow"),
                },
                _ => panic!("expected PermissionRequest"),
            },
            _ => panic!("expected sync"),
        }
    }

    #[test]
    fn rejects_values_claude_schema_rejects() {
        assert!(parse_hook_json(r#"{"decision":"maybe"}"#).is_err());
        assert!(parse_hook_json(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","permissionDecision":"maybe"}}"#
        )
        .is_err());
        assert!(parse_hook_json(
            r#"{"hookSpecificOutput":{"hookEventName":"PreToolUse","updatedInput":[]}}"#
        )
        .is_err());
        assert!(parse_hook_json(
            r#"{"hookSpecificOutput":{"hookEventName":"Elicitation","action":"maybe"}}"#
        )
        .is_err());
        assert!(
            parse_hook_json(r#"{"hookSpecificOutput":{"hookEventName":"PermissionRequest"}}"#)
                .is_err()
        );
    }

    #[test]
    fn every_hook_specific_output_variant_parses() {
        let cases = [
            ("PreToolUse", r#"{"hookEventName":"PreToolUse"}"#),
            (
                "UserPromptSubmit",
                r#"{"hookEventName":"UserPromptSubmit"}"#,
            ),
            ("SessionStart", r#"{"hookEventName":"SessionStart"}"#),
            ("Setup", r#"{"hookEventName":"Setup"}"#),
            ("SubagentStart", r#"{"hookEventName":"SubagentStart"}"#),
            ("PostToolUse", r#"{"hookEventName":"PostToolUse"}"#),
            (
                "PostToolUseFailure",
                r#"{"hookEventName":"PostToolUseFailure"}"#,
            ),
            (
                "PermissionDenied",
                r#"{"hookEventName":"PermissionDenied"}"#,
            ),
            ("Notification", r#"{"hookEventName":"Notification"}"#),
            (
                "PermissionRequest",
                r#"{"hookEventName":"PermissionRequest","decision":{"behavior":"deny"}}"#,
            ),
            ("CwdChanged", r#"{"hookEventName":"CwdChanged"}"#),
            ("FileChanged", r#"{"hookEventName":"FileChanged"}"#),
            ("Elicitation", r#"{"hookEventName":"Elicitation"}"#),
            (
                "ElicitationResult",
                r#"{"hookEventName":"ElicitationResult"}"#,
            ),
            (
                "WorktreeCreate",
                r#"{"hookEventName":"WorktreeCreate","worktreePath":"/tmp/tree"}"#,
            ),
        ];

        for (expected, specific) in cases {
            let raw = format!(r#"{{"hookSpecificOutput":{specific}}}"#);
            let HookJsonOutput::Sync(output) = parse_hook_json(&raw).unwrap() else {
                panic!("expected sync output");
            };
            assert_eq!(output.hook_specific_output.unwrap().event_name(), expected);
        }
    }
}
