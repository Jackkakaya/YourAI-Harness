//! Validate Hook definitions and build runtime registrations.
pub use crate::config::{FailurePolicyConfig, HandlerConfig, HookShell};
use crate::hooks::matcher::CompiledMatcher;
use std::time::Duration;
pub use yourai_core::hooks::{FailurePolicy, HookSource};

impl From<FailurePolicyConfig> for FailurePolicy {
    fn from(value: FailurePolicyConfig) -> Self {
        match value {
            FailurePolicyConfig::Open => FailurePolicy::Open,
            FailurePolicyConfig::Closed => FailurePolicy::Closed,
        }
    }
}

impl HandlerConfig {
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

    /// Validate once and return the resolved timeout for registration.
    pub fn validate(&self) -> Result<Option<Duration>, String> {
        let timeout = match self {
            HandlerConfig::Command { timeout, .. }
            | HandlerConfig::Http { timeout, .. }
            | HandlerConfig::Prompt { timeout, .. }
            | HandlerConfig::Agent { timeout, .. } => *timeout,
        }
        .map(|seconds| {
            if !seconds.is_finite() || seconds <= 0.0 {
                return Err("hook timeout must be a positive finite number".to_string());
            }
            let duration = Duration::try_from_secs_f64(seconds)
                .map_err(|_| "hook timeout exceeds Duration range".to_string())?;
            if duration.is_zero() {
                return Err("hook timeout is below Duration resolution".to_string());
            }
            Ok(duration)
        })
        .transpose()?;
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
        Ok(timeout)
    }
}

/// 内部注册项：编译后的 matcher + handler 配置 + 元数据。
#[derive(Debug, Clone)]
pub struct HookRegistration {
    pub id: String,
    pub event: yourai_core::hooks::HookEventKind,
    pub matcher: CompiledMatcher,
    pub handler: HandlerConfig,
    pub timeout: Option<Duration>,
    pub source: HookSource,
    pub failure_policy: FailurePolicy,
    pub once: bool,
}

/// 从配置构建注册项列表。
pub fn build_registrations(
    config: &serde_json::Value,
    source: HookSource,
) -> Result<Vec<HookRegistration>, String> {
    let mut regs = Vec::new();
    let groups = config
        .get("hooks")
        .unwrap_or(config)
        .as_object()
        .ok_or("hooks must be an object")?;
    for (event_name, groups) in groups {
        let groups = groups
            .as_array()
            .ok_or_else(|| format!("invalid hook groups for {event_name}"))?;
        let Some(event) = yourai_core::hooks::HookEventKind::from_name(event_name) else {
            return Err(format!("unknown hook event: {event_name}"));
        };
        for (gi, group) in groups.iter().enumerate() {
            let pattern: Option<String> = serde_json::from_value(group["matcher"].clone())
                .map_err(|_| format!("invalid hook matcher for {event_name}"))?;
            let handlers: Vec<HandlerConfig> = serde_json::from_value(group["hooks"].clone())
                .map_err(|_| format!("invalid hook handlers for {event_name}"))?;
            let matcher = match pattern.as_deref() {
                Some(pattern) => CompiledMatcher::try_compile(pattern)?,
                None => CompiledMatcher::All,
            };
            for (hi, handler) in handlers.iter().enumerate() {
                let timeout = handler.validate()?;
                let id = format!("{}:{}:{}:{}", source.as_str(), event_name, gi, hi);
                let failure_policy = handler.failure_policy();
                let once = handler.once();
                regs.push(HookRegistration {
                    id,
                    event,
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
    Ok(regs)
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
        let config: serde_json::Value = serde_json::from_str(json).unwrap();
        assert_eq!(config["hooks"].as_object().unwrap().len(), 2);
        let pre = config["hooks"].get("PreToolUse").unwrap();
        assert_eq!(pre.as_array().unwrap().len(), 1);
        assert_eq!(pre[0]["matcher"].as_str(), Some("Read|Write"));
        assert_eq!(pre[0]["hooks"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn build_regs() {
        let json = r#"{"hooks":{"PreToolUse":[{"matcher":"Bash","hooks":[{"type":"command","command":"echo hi"}]}]}}"#;
        let config: serde_json::Value = serde_json::from_str(json).unwrap();
        let regs = build_registrations(&config, HookSource::User).unwrap();
        assert_eq!(regs.len(), 1);
        assert_eq!(regs[0].event, yourai_core::hooks::HookEventKind::PreToolUse);
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
        let config: serde_json::Value = serde_json::from_str(json).unwrap();
        let command: HandlerConfig =
            serde_json::from_value(config["hooks"]["PreToolUse"][0]["hooks"][0].clone()).unwrap();
        assert!(command.is_async());
        assert!(command.once());
        let http: HandlerConfig =
            serde_json::from_value(config["hooks"]["PostToolUse"][0]["hooks"][0].clone()).unwrap();
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
    fn validates_timeout_url_and_model_configuration() {
        let bad_timeout: HandlerConfig =
            serde_json::from_str(r#"{"type":"command","command":"check","timeout":0}"#).unwrap();
        assert!(bad_timeout.validate().is_err());

        let bad_url: HandlerConfig =
            serde_json::from_str(r#"{"type":"http","url":"file:///tmp/hook"}"#).unwrap();
        assert!(bad_url.validate().is_err());

        let prompt: HandlerConfig =
            serde_json::from_str(r#"{"type":"prompt","prompt":"review"}"#).unwrap();
        assert!(prompt.validate().is_ok());
    }

    #[test]
    fn registration_builder_rejects_invalid_matcher_and_timeout() {
        let bad_matcher: serde_json::Value = serde_json::from_str(
            r#"{"hooks":{"PreToolUse":[{"matcher":"[","hooks":[{"type":"command","command":"check"}]}]}}"#,
        )
        .unwrap();
        assert!(build_registrations(&bad_matcher, HookSource::Project).is_err());

        let bad_timeout: serde_json::Value = serde_json::from_str(
            r#"{"hooks":{"PreToolUse":[{"hooks":[{"type":"command","command":"check","timeout":-1}]}]}}"#,
        )
        .unwrap();
        assert!(build_registrations(&bad_timeout, HookSource::Project).is_err());
    }

    #[test]
    fn registration_resolves_timeouts_and_rejects_unrepresentable_values() {
        for handler in [
            serde_json::json!({"type":"command", "command":"check"}),
            serde_json::json!({"type":"http", "url":"https://example.com/hook"}),
            serde_json::json!({"type":"prompt", "prompt":"check"}),
            serde_json::json!({"type":"agent", "prompt":"check"}),
        ] {
            for (seconds, expected) in [
                (None, Ok(None)),
                (Some(1.25), Ok(Some(Duration::from_millis(1250)))),
                (Some(0.0), Err(())),
                (Some(-1.0), Err(())),
                (Some(1e300), Err(())),
                (Some(1e-300), Err(())),
            ] {
                let mut handler = handler.clone();
                if let Some(seconds) = seconds {
                    handler["timeout"] = serde_json::json!(seconds);
                }
                let config: serde_json::Value = serde_json::from_value(serde_json::json!({
                    "hooks": {"PreToolUse": [{"hooks": [handler]}]}
                }))
                .unwrap();
                let result = build_registrations(&config, HookSource::Project);
                match expected {
                    Ok(timeout) => assert_eq!(result.unwrap()[0].timeout, timeout),
                    Err(()) => assert!(result.is_err(), "invalid timeout {seconds:?} accepted"),
                }
            }
        }
    }
}
