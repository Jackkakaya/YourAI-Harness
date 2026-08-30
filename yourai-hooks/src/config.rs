//! Hook 配置类型：兼容 Claude `settings.json` 的 hooks 分组格式。
//!
//! 配置示例：
//! ```json
//! {
//!   "hooks": {
//!     "PreToolUse": [
//!       { "matcher": "Read|Write", "hooks": [ { "type": "command", "command": "..." } ] }
//!     ]
//!   }
//! }
//! ```

use crate::handler::HookHandlerKind;
use crate::matcher::CompiledMatcher;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::Duration;
pub use yourai_core::hooks::{FailurePolicy, HookSource};

/// 顶层 hooks 配置。
#[derive(Debug, Clone, Deserialize, Serialize, Default)]
pub struct HooksConfig {
    #[serde(default)]
    pub hooks: HashMap<String, Vec<HookMatcherGroup>>,
}

/// 一个 matcher group：可选 matcher + 一组 handler。
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct HookMatcherGroup {
    #[serde(default)]
    pub matcher: Option<String>,
    pub hooks: Vec<HandlerConfig>,
}

/// Handler 配置（discriminated union on `type`）。
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "type")]
pub enum HandlerConfig {
    #[serde(rename = "command")]
    Command {
        command: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        shell: Option<HookShell>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "async")]
        is_async: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "asyncRewake"
        )]
        async_rewake: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "statusMessage"
        )]
        status_message: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "failurePolicy"
        )]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "http")]
    Http {
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        headers: Option<HashMap<String, String>>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "allowedEnvVars"
        )]
        allowed_env_vars: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "statusMessage"
        )]
        status_message: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "failurePolicy"
        )]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "prompt")]
    Prompt {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "statusMessage"
        )]
        status_message: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "failurePolicy"
        )]
        failure_policy: Option<FailurePolicyConfig>,
    },
    #[serde(rename = "agent")]
    Agent {
        prompt: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        timeout: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        model: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none", rename = "if")]
        if_condition: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        once: Option<bool>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "statusMessage"
        )]
        status_message: Option<String>,
        #[serde(
            default,
            skip_serializing_if = "Option::is_none",
            rename = "failurePolicy"
        )]
        failure_policy: Option<FailurePolicyConfig>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FailurePolicyConfig {
    Open,
    Closed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HookShell {
    Bash,
    Powershell,
}

impl From<FailurePolicyConfig> for FailurePolicy {
    fn from(value: FailurePolicyConfig) -> Self {
        match value {
            FailurePolicyConfig::Open => FailurePolicy::Open,
            FailurePolicyConfig::Closed => FailurePolicy::Closed,
        }
    }
}

impl HandlerConfig {
    pub fn timeout(&self) -> Option<Duration> {
        let secs = match self {
            HandlerConfig::Command { timeout, .. }
            | HandlerConfig::Http { timeout, .. }
            | HandlerConfig::Prompt { timeout, .. }
            | HandlerConfig::Agent { timeout, .. } => *timeout,
        };
        secs.map(Duration::from_secs_f64)
    }

    pub fn handler_kind(&self) -> HookHandlerKind {
        match self {
            HandlerConfig::Command { .. } => HookHandlerKind::Command,
            HandlerConfig::Http { .. } => HookHandlerKind::Http,
            HandlerConfig::Prompt { .. } => HookHandlerKind::Prompt,
            HandlerConfig::Agent { .. } => HookHandlerKind::Agent,
        }
    }

    pub fn if_condition(&self) -> Option<&str> {
        match self {
            HandlerConfig::Command { if_condition, .. }
            | HandlerConfig::Http { if_condition, .. }
            | HandlerConfig::Prompt { if_condition, .. }
            | HandlerConfig::Agent { if_condition, .. } => if_condition.as_deref(),
        }
    }

    pub fn once(&self) -> bool {
        match self {
            HandlerConfig::Command { once, .. }
            | HandlerConfig::Http { once, .. }
            | HandlerConfig::Prompt { once, .. }
            | HandlerConfig::Agent { once, .. } => once.unwrap_or(false),
        }
    }

    pub fn failure_policy(&self) -> FailurePolicy {
        let policy = match self {
            HandlerConfig::Command { failure_policy, .. }
            | HandlerConfig::Http { failure_policy, .. }
            | HandlerConfig::Prompt { failure_policy, .. }
            | HandlerConfig::Agent { failure_policy, .. } => *failure_policy,
        };
        policy.map(Into::into).unwrap_or_default()
    }

    pub fn is_async(&self) -> bool {
        match self {
            HandlerConfig::Command {
                is_async,
                async_rewake,
                ..
            } => is_async.unwrap_or(false) || async_rewake.unwrap_or(false),
            _ => false,
        }
    }

    pub fn async_rewake(&self) -> bool {
        match self {
            HandlerConfig::Command { async_rewake, .. } => async_rewake.unwrap_or(false),
            _ => false,
        }
    }

    pub fn status_message(&self) -> Option<&str> {
        match self {
            HandlerConfig::Command { status_message, .. }
            | HandlerConfig::Http { status_message, .. }
            | HandlerConfig::Prompt { status_message, .. }
            | HandlerConfig::Agent { status_message, .. } => status_message.as_deref(),
        }
    }

    /// 该 handler 是否要求宿主注入模型执行能力。
    pub fn requires_model_executor(&self) -> bool {
        matches!(
            self,
            HandlerConfig::Prompt { .. } | HandlerConfig::Agent { .. }
        )
    }

    pub fn validate(&self) -> Result<(), String> {
        if let Some(timeout) = match self {
            HandlerConfig::Command { timeout, .. }
            | HandlerConfig::Http { timeout, .. }
            | HandlerConfig::Prompt { timeout, .. }
            | HandlerConfig::Agent { timeout, .. } => *timeout,
        } {
            if !timeout.is_finite() || timeout <= 0.0 {
                return Err("hook timeout must be a positive finite number".to_string());
            }
        }
        match self {
            HandlerConfig::Http { url, .. } => {
                let parsed = reqwest::Url::parse(url)
                    .map_err(|error| format!("invalid HTTP hook URL '{url}': {error}"))?;
                if !matches!(parsed.scheme(), "http" | "https") {
                    return Err(format!("HTTP hook URL must use http or https: {url}"));
                }
            }
            HandlerConfig::Command { .. }
            | HandlerConfig::Prompt { .. }
            | HandlerConfig::Agent { .. } => {}
        }
        Ok(())
    }
}

/// 内部注册项：编译后的 matcher + handler 配置 + 元数据。
#[derive(Debug, Clone)]
pub struct HookRegistration {
    pub id: String,
    pub event_name: String,
    pub matcher: CompiledMatcher,
    pub handler: HandlerConfig,
    pub timeout: Option<Duration>,
    pub source: HookSource,
    pub failure_policy: FailurePolicy,
    pub once: bool,
}

/// 从配置构建注册项列表。
pub fn build_registrations(config: &HooksConfig, source: HookSource) -> Vec<HookRegistration> {
    let mut regs = Vec::new();
    for (event_name, groups) in &config.hooks {
        for (gi, group) in groups.iter().enumerate() {
            let matcher = group
                .matcher
                .as_deref()
                .map(CompiledMatcher::compile)
                .unwrap_or(CompiledMatcher::All);
            for (hi, handler) in group.hooks.iter().enumerate() {
                let id = format!("{}:{}:{}:{}", source.as_str(), event_name, gi, hi);
                let timeout = handler.timeout();
                let failure_policy = handler.failure_policy();
                let once = handler.once();
                regs.push(HookRegistration {
                    id,
                    event_name: event_name.clone(),
                    matcher: matcher.clone(),
                    handler: handler.clone(),
                    timeout,
                    source: source.clone(),
                    failure_policy,
                    once,
                });
            }
        }
    }
    regs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_config() {
        let json = r#"{
          "hooks": {
            "PreToolUse": [
              { "matcher": "Read|Write", "hooks": [
                { "type": "command", "command": "python3 hook.py", "timeout": 10 }
              ]}
            ],
            "UserPromptSubmit": [
              { "hooks": [
                { "type": "http", "url": "https://example.com/hook", "timeout": 5 }
              ]}
            ]
          }
        }"#;
        let config: HooksConfig = serde_json::from_str(json).unwrap();
        assert_eq!(config.hooks.len(), 2);
        let pre = config.hooks.get("PreToolUse").unwrap();
        assert_eq!(pre.len(), 1);
        assert_eq!(pre[0].matcher.as_deref(), Some("Read|Write"));
        assert_eq!(pre[0].hooks.len(), 1);
    }

    #[test]
    fn build_regs() {
        let json = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}"#;
        let config: HooksConfig = serde_json::from_str(json).unwrap();
        let regs = build_registrations(&config, HookSource::User);
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].event_name, "PreToolUse");
        assert_eq!(regs[0].id, "user:PreToolUse:0:0");
        assert!(regs[0].matcher.matches("Bash"));
        assert!(!regs[0].matcher.matches("Read"));
    }

    #[test]
    fn parses_claude_camel_case_fields() {
        let json = r#"{
          "hooks": {
            "PreToolUse": [{"hooks": [{
              "type": "command",
              "command": "check",
              "asyncRewake": true,
              "statusMessage": "Checking",
              "once": true
            }]}],
            "PostToolUse": [{"hooks": [{
              "type": "http",
              "url": "https://example.com/hook",
              "allowedEnvVars": ["TOKEN"],
              "if": "Bash(git *)",
              "statusMessage": "Tracing"
            }]}]
          }
        }"#;
        let config: HooksConfig = serde_json::from_str(json).unwrap();
        let command = &config.hooks["PreToolUse"][0].hooks[0];
        assert!(command.is_async());
        assert!(command.once());
        let http = &config.hooks["PostToolUse"][0].hooks[0];
        assert_eq!(http.if_condition(), Some("Bash(git *)"));
        match http {
            HandlerConfig::Http {
                allowed_env_vars,
                status_message,
                ..
            } => {
                assert_eq!(
                    allowed_env_vars.as_deref(),
                    Some(["TOKEN".to_string()].as_slice())
                );
                assert_eq!(status_message.as_deref(), Some("Tracing"));
            }
            _ => panic!("expected HTTP handler"),
        }
    }

    #[test]
    fn validates_timeout_url_and_marks_model_handlers() {
        let bad_timeout: HandlerConfig =
            serde_json::from_str(r#"{"type":"command","command":"check","timeout":0}"#).unwrap();
        assert!(bad_timeout.validate().is_err());

        let bad_url: HandlerConfig =
            serde_json::from_str(r#"{"type":"http","url":"file:///tmp/hook"}"#).unwrap();
        assert!(bad_url.validate().is_err());

        let prompt: HandlerConfig =
            serde_json::from_str(r#"{"type":"prompt","prompt":"review"}"#).unwrap();
        assert!(prompt.validate().is_ok());
        assert!(prompt.requires_model_executor());
    }
}
