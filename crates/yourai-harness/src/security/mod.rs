use crate::{
    error,
    storage::{atomic_write, read_json},
};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::{
    path::PathBuf,
    sync::{Arc, Mutex},
};
use yourai_core::prelude::*;
#[derive(Clone, Default, Serialize, Deserialize)]
struct Policy {
    allow: Vec<String>,
    deny: Vec<String>,
    ask: Vec<String>,
}

/// Explicit per-run local permission bypass. Never rewrites persisted rules.
pub struct YoloSecurity;
impl SecurityProvider for YoloSecurity {
    fn bypass_approvals(&self) -> bool {
        true
    }
    fn check_tool_call<'a>(
        &'a self,
        _: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async { Ok(ApprovalDecision::Allow) })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Allow) })
    }
    fn check_file_access<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async { Ok(PolicyDecision::Allow) })
    }
}
/// Persistent exact tool-name rules. Hard deny configuration cannot be removed by hooks.
pub struct PolicySecurity {
    path: PathBuf,
    rules: Mutex<Policy>,
    hard_deny: Vec<String>,
    workspace: Option<PathBuf>,
    trusted_shell: bool,
}
impl PolicySecurity {
    pub fn open(path: PathBuf, hard_deny: Vec<String>) -> Result<Arc<Self>, YourAiError> {
        Self::open_policy(path, hard_deny, None, false)
    }
    pub fn open_for_workspace(
        path: PathBuf,
        hard_deny: Vec<String>,
        cwd: PathBuf,
        trusted_shell: bool,
    ) -> Result<Arc<Self>, YourAiError> {
        let cwd = std::fs::canonicalize(cwd).map_err(|e| error("security", e))?;
        Self::open_policy(path, hard_deny, Some(cwd), trusted_shell)
    }
    fn open_policy(
        path: PathBuf,
        hard_deny: Vec<String>,
        workspace: Option<PathBuf>,
        trusted_shell: bool,
    ) -> Result<Arc<Self>, YourAiError> {
        let rules = if path.exists() {
            read_json(&path)?
        } else {
            Policy::default()
        };
        Ok(Arc::new(Self {
            path,
            rules: Mutex::new(rules),
            hard_deny,
            workspace,
            trusted_shell,
        }))
    }
}
impl SecurityProvider for PolicySecurity {
    fn update_permissions<'a>(
        &'a self,
        updates: &'a [Value],
    ) -> BoxFuture<'a, Result<(), YourAiError>> {
        Box::pin(async move {
            let mut guard = self.rules.lock().unwrap();
            let mut next = guard.clone();
            for update in updates {
                if update
                    .get("destination")
                    .is_some_and(|d| d.as_str() != Some("session"))
                {
                    return Err(error(
                        "security",
                        "only session permission destination is supported",
                    ));
                }
                let kind = update["type"].as_str().unwrap_or("");
                if kind != "addRules" && kind != "removeRules" && kind != "replaceRules" {
                    return Err(error("security", "unsupported permission update type"));
                }
                let rules = update["rules"]
                    .as_array()
                    .ok_or_else(|| error("security", "rules must be an array"))?;
                let target = match update["behavior"].as_str() {
                    Some("allow") => &mut next.allow,
                    Some("deny") => &mut next.deny,
                    Some("ask") => &mut next.ask,
                    _ => return Err(error("security", "invalid rule behavior")),
                };
                if kind == "replaceRules" {
                    target.clear();
                }
                for rule in rules {
                    let name = rule["toolName"]
                        .as_str()
                        .ok_or_else(|| error("security", "rule requires toolName"))?;
                    if rule.get("ruleContent").is_some_and(|v| !v.is_null()) {
                        return Err(error(
                            "security",
                            "scoped rule patterns unsupported; refusing broader grant",
                        ));
                    }
                    if kind == "addRules" || kind == "replaceRules" {
                        if !target.iter().any(|n| n == name) {
                            target.push(name.into());
                        }
                    } else {
                        target.retain(|n| n != name);
                    }
                }
            }
            atomic_write(&self.path, &next)?;
            *guard = next;
            Ok(())
        })
    }
    fn check_tool_call<'a>(
        &'a self,
        c: &'a SecurityContext,
    ) -> BoxFuture<'a, Result<ApprovalDecision, YourAiError>> {
        Box::pin(async move {
            let p = self.rules.lock().unwrap();
            Ok(
                if self.hard_deny.contains(&c.action) || p.deny.contains(&c.action) {
                    ApprovalDecision::Deny
                } else if p.ask.contains(&c.action) {
                    ApprovalDecision::Ask
                } else if p.allow.contains(&c.action) {
                    ApprovalDecision::Allow
                } else if c
                    .input
                    .get("path_resolution_failed")
                    .and_then(Value::as_bool)
                    == Some(true)
                {
                    ApprovalDecision::Deny
                } else if self.workspace.as_ref().is_some_and(|root| {
                    matches!(c.action.as_str(), "read" | "write" | "edit")
                        && c.input
                            .get("path")
                            .and_then(Value::as_str)
                            .and_then(|p| {
                                crate::tools::resolve_path(root, std::path::Path::new(p)).ok()
                            })
                            .is_some_and(|p| p.starts_with(root))
                }) || (self.workspace.is_some()
                    && self.trusted_shell
                    && c.action == "shell")
                {
                    ApprovalDecision::Allow
                } else {
                    ApprovalDecision::Ask
                },
            )
        })
    }
    fn check_command<'a>(
        &'a self,
        _: &'a str,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async {
            Ok(if self.workspace.is_some() {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            })
        })
    }
    fn check_file_access<'a>(
        &'a self,
        _: &'a str,
        _: bool,
    ) -> BoxFuture<'a, Result<PolicyDecision, YourAiError>> {
        Box::pin(async {
            Ok(if self.workspace.is_some() {
                PolicyDecision::Allow
            } else {
                PolicyDecision::Deny
            })
        })
    }
}
