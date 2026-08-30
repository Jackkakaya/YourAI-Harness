//! Wire JSON 序列化：把 `HookInvocation` 序列化为 Claude 兼容的 snake_case JSON。
//!
//! 类型定义在 `yourai-core::hooks`；本模块只负责序列化实现。

use serde_json::{json, Map, Value};
use yourai_core::hooks::{HookEvent, HookInvocation};

/// 把 `HookInvocation` 序列化为 Claude 兼容的 wire JSON（snake_case，全顶层）。
pub fn to_wire_json(invocation: &HookInvocation) -> Value {
    let mut map = Map::new();
    let b = &invocation.base;
    map.insert("session_id".into(), json!(b.session_id));
    map.insert("transcript_path".into(), json!(b.transcript_path));
    map.insert("cwd".into(), json!(b.cwd));
    if let Some(v) = &b.permission_mode {
        map.insert("permission_mode".into(), json!(v));
    }
    if let Some(v) = &b.agent_id {
        map.insert("agent_id".into(), json!(v));
    }
    if let Some(v) = &b.agent_type {
        map.insert("agent_type".into(), json!(v));
    }
    map.insert(
        "hook_event_name".into(),
        json!(invocation.event.event_name()),
    );
    serialize_event_fields(&invocation.event, &mut map);
    Value::Object(map)
}

fn serialize_event_fields(event: &HookEvent, map: &mut Map<String, Value>) {
    match event {
        HookEvent::PreToolUse {
            tool_name,
            tool_input,
            tool_use_id,
        } => {
            map.insert("tool_name".into(), json!(tool_name));
            map.insert("tool_input".into(), tool_input.clone());
            map.insert("tool_use_id".into(), json!(tool_use_id));
        }
        HookEvent::PostToolUse {
            tool_name,
            tool_input,
            tool_response,
            tool_use_id,
        } => {
            map.insert("tool_name".into(), json!(tool_name));
            map.insert("tool_input".into(), tool_input.clone());
            map.insert("tool_response".into(), tool_response.clone());
            map.insert("tool_use_id".into(), json!(tool_use_id));
        }
        HookEvent::PostToolUseFailure {
            tool_name,
            tool_input,
            tool_use_id,
            error,
            is_interrupt,
        } => {
            map.insert("tool_name".into(), json!(tool_name));
            map.insert("tool_input".into(), tool_input.clone());
            map.insert("tool_use_id".into(), json!(tool_use_id));
            map.insert("error".into(), json!(error));
            if let Some(v) = is_interrupt {
                map.insert("is_interrupt".into(), json!(v));
            }
        }
        HookEvent::PermissionRequest {
            tool_name,
            tool_input,
            permission_suggestions,
        } => {
            map.insert("tool_name".into(), json!(tool_name));
            map.insert("tool_input".into(), tool_input.clone());
            if let Some(v) = permission_suggestions {
                map.insert("permission_suggestions".into(), json!(v));
            }
        }
        HookEvent::PermissionDenied {
            tool_name,
            tool_input,
            tool_use_id,
            reason,
        } => {
            map.insert("tool_name".into(), json!(tool_name));
            map.insert("tool_input".into(), tool_input.clone());
            map.insert("tool_use_id".into(), json!(tool_use_id));
            map.insert("reason".into(), json!(reason));
        }
        HookEvent::Notification {
            message,
            title,
            notification_type,
        } => {
            map.insert("message".into(), json!(message));
            if let Some(v) = title {
                map.insert("title".into(), json!(v));
            }
            map.insert("notification_type".into(), json!(notification_type));
        }
        HookEvent::UserPromptSubmit { prompt } => {
            map.insert("prompt".into(), json!(prompt));
        }
        HookEvent::SessionStart { source, model } => {
            map.insert("source".into(), json!(source));
            if let Some(v) = model {
                map.insert("model".into(), json!(v));
            }
        }
        HookEvent::SessionEnd { reason } => {
            map.insert("reason".into(), json!(reason));
        }
        HookEvent::Stop {
            stop_hook_active,
            last_assistant_message,
        } => {
            map.insert("stop_hook_active".into(), json!(stop_hook_active));
            if let Some(v) = last_assistant_message {
                map.insert("last_assistant_message".into(), json!(v));
            }
        }
        HookEvent::StopFailure {
            error,
            error_details,
            last_assistant_message,
        } => {
            map.insert("error".into(), json!(error));
            if let Some(v) = error_details {
                map.insert("error_details".into(), json!(v));
            }
            if let Some(v) = last_assistant_message {
                map.insert("last_assistant_message".into(), json!(v));
            }
        }
        HookEvent::SubagentStart {
            agent_id,
            agent_type,
        } => {
            map.insert("agent_id".into(), json!(agent_id));
            map.insert("agent_type".into(), json!(agent_type));
        }
        HookEvent::SubagentStop {
            stop_hook_active,
            agent_id,
            agent_transcript_path,
            agent_type,
            last_assistant_message,
        } => {
            map.insert("stop_hook_active".into(), json!(stop_hook_active));
            map.insert("agent_id".into(), json!(agent_id));
            map.insert("agent_transcript_path".into(), json!(agent_transcript_path));
            map.insert("agent_type".into(), json!(agent_type));
            if let Some(v) = last_assistant_message {
                map.insert("last_assistant_message".into(), json!(v));
            }
        }
        HookEvent::PreCompact {
            trigger,
            custom_instructions,
        } => {
            map.insert("trigger".into(), json!(trigger));
            map.insert("custom_instructions".into(), json!(custom_instructions));
        }
        HookEvent::PostCompact {
            trigger,
            compact_summary,
        } => {
            map.insert("trigger".into(), json!(trigger));
            map.insert("compact_summary".into(), json!(compact_summary));
        }
        HookEvent::Setup { trigger } => {
            map.insert("trigger".into(), json!(trigger));
        }
        HookEvent::TeammateIdle {
            teammate_name,
            team_name,
        } => {
            map.insert("teammate_name".into(), json!(teammate_name));
            map.insert("team_name".into(), json!(team_name));
        }
        HookEvent::TaskCreated {
            task_id,
            task_subject,
            task_description,
            teammate_name,
            team_name,
        }
        | HookEvent::TaskCompleted {
            task_id,
            task_subject,
            task_description,
            teammate_name,
            team_name,
        } => {
            map.insert("task_id".into(), json!(task_id));
            map.insert("task_subject".into(), json!(task_subject));
            if let Some(v) = task_description {
                map.insert("task_description".into(), json!(v));
            }
            if let Some(v) = teammate_name {
                map.insert("teammate_name".into(), json!(v));
            }
            if let Some(v) = team_name {
                map.insert("team_name".into(), json!(v));
            }
        }
        HookEvent::Elicitation {
            mcp_server_name,
            message,
            mode,
            url,
            elicitation_id,
            requested_schema,
        } => {
            map.insert("mcp_server_name".into(), json!(mcp_server_name));
            map.insert("message".into(), json!(message));
            if let Some(v) = mode {
                map.insert("mode".into(), json!(v));
            }
            if let Some(v) = url {
                map.insert("url".into(), json!(v));
            }
            if let Some(v) = elicitation_id {
                map.insert("elicitation_id".into(), json!(v));
            }
            if let Some(v) = requested_schema {
                map.insert("requested_schema".into(), v.clone());
            }
        }
        HookEvent::ElicitationResult {
            mcp_server_name,
            elicitation_id,
            mode,
            action,
            content,
        } => {
            map.insert("mcp_server_name".into(), json!(mcp_server_name));
            if let Some(v) = elicitation_id {
                map.insert("elicitation_id".into(), json!(v));
            }
            if let Some(v) = mode {
                map.insert("mode".into(), json!(v));
            }
            map.insert("action".into(), json!(action));
            if let Some(v) = content {
                map.insert("content".into(), v.clone());
            }
        }
        HookEvent::ConfigChange { source, file_path } => {
            map.insert("source".into(), json!(source));
            if let Some(v) = file_path {
                map.insert("file_path".into(), json!(v));
            }
        }
        HookEvent::InstructionsLoaded {
            file_path,
            memory_type,
            load_reason,
            globs,
            trigger_file_path,
            parent_file_path,
        } => {
            map.insert("file_path".into(), json!(file_path));
            map.insert("memory_type".into(), json!(memory_type));
            map.insert("load_reason".into(), json!(load_reason));
            if let Some(v) = globs {
                map.insert("globs".into(), json!(v));
            }
            if let Some(v) = trigger_file_path {
                map.insert("trigger_file_path".into(), json!(v));
            }
            if let Some(v) = parent_file_path {
                map.insert("parent_file_path".into(), json!(v));
            }
        }
        HookEvent::WorktreeCreate { name } => {
            map.insert("name".into(), json!(name));
        }
        HookEvent::WorktreeRemove { worktree_path } => {
            map.insert("worktree_path".into(), json!(worktree_path));
        }
        HookEvent::CwdChanged { old_cwd, new_cwd } => {
            map.insert("old_cwd".into(), json!(old_cwd));
            map.insert("new_cwd".into(), json!(new_cwd));
        }
        HookEvent::FileChanged { file_path, event } => {
            map.insert("file_path".into(), json!(file_path));
            map.insert("event".into(), json!(event));
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use yourai_core::hooks::BaseInput;

    #[test]
    fn wire_json_has_snake_case_fields() {
        let inv = HookInvocation::new(
            BaseInput::new("sess_01", "/workspace"),
            HookEvent::PreToolUse {
                tool_name: "Bash".to_string(),
                tool_input: json!({"command": "ls"}),
                tool_use_id: "call_01".to_string(),
            },
        );
        let wire = to_wire_json(&inv);
        assert_eq!(wire["hook_event_name"], "PreToolUse");
        assert_eq!(wire["tool_name"], "Bash");
        assert_eq!(wire["tool_use_id"], "call_01");
        assert_eq!(wire["session_id"], "sess_01");
        assert_eq!(wire["cwd"], "/workspace");
    }

    #[test]
    fn every_supported_event_serializes_its_discriminator() {
        let text = || "x".to_string();
        let events = vec![
            HookEvent::PreToolUse {
                tool_name: text(),
                tool_input: json!({}),
                tool_use_id: text(),
            },
            HookEvent::PostToolUse {
                tool_name: text(),
                tool_input: json!({}),
                tool_response: json!({}),
                tool_use_id: text(),
            },
            HookEvent::PostToolUseFailure {
                tool_name: text(),
                tool_input: json!({}),
                tool_use_id: text(),
                error: text(),
                is_interrupt: Some(false),
            },
            HookEvent::PermissionRequest {
                tool_name: text(),
                tool_input: json!({}),
                permission_suggestions: Some(vec![]),
            },
            HookEvent::PermissionDenied {
                tool_name: text(),
                tool_input: json!({}),
                tool_use_id: text(),
                reason: text(),
            },
            HookEvent::Notification {
                message: text(),
                title: Some(text()),
                notification_type: text(),
            },
            HookEvent::UserPromptSubmit { prompt: text() },
            HookEvent::SessionStart {
                source: text(),
                model: Some(text()),
            },
            HookEvent::SessionEnd { reason: text() },
            HookEvent::Stop {
                stop_hook_active: false,
                last_assistant_message: Some(text()),
            },
            HookEvent::StopFailure {
                error: text(),
                error_details: Some(text()),
                last_assistant_message: Some(text()),
            },
            HookEvent::SubagentStart {
                agent_id: text(),
                agent_type: text(),
            },
            HookEvent::SubagentStop {
                stop_hook_active: false,
                agent_id: text(),
                agent_transcript_path: text(),
                agent_type: text(),
                last_assistant_message: Some(text()),
            },
            HookEvent::PreCompact {
                trigger: text(),
                custom_instructions: Some(text()),
            },
            HookEvent::PostCompact {
                trigger: text(),
                compact_summary: text(),
            },
            HookEvent::Setup { trigger: text() },
            HookEvent::TeammateIdle {
                teammate_name: text(),
                team_name: text(),
            },
            HookEvent::TaskCreated {
                task_id: text(),
                task_subject: text(),
                task_description: Some(text()),
                teammate_name: Some(text()),
                team_name: Some(text()),
            },
            HookEvent::TaskCompleted {
                task_id: text(),
                task_subject: text(),
                task_description: Some(text()),
                teammate_name: Some(text()),
                team_name: Some(text()),
            },
            HookEvent::Elicitation {
                mcp_server_name: text(),
                message: text(),
                mode: Some(text()),
                url: Some(text()),
                elicitation_id: Some(text()),
                requested_schema: Some(json!({})),
            },
            HookEvent::ElicitationResult {
                mcp_server_name: text(),
                elicitation_id: Some(text()),
                mode: Some(text()),
                action: text(),
                content: Some(json!({})),
            },
            HookEvent::ConfigChange {
                source: text(),
                file_path: Some(text()),
            },
            HookEvent::InstructionsLoaded {
                file_path: text(),
                memory_type: text(),
                load_reason: text(),
                globs: Some(vec![text()]),
                trigger_file_path: Some(text()),
                parent_file_path: Some(text()),
            },
            HookEvent::WorktreeCreate { name: text() },
            HookEvent::WorktreeRemove {
                worktree_path: text(),
            },
            HookEvent::CwdChanged {
                old_cwd: text(),
                new_cwd: text(),
            },
            HookEvent::FileChanged {
                file_path: text(),
                event: text(),
            },
        ];

        assert_eq!(events.len(), yourai_core::hooks::HookEventKind::ALL.len());
        for event in events {
            let expected = event.event_name();
            let wire = to_wire_json(&HookInvocation::new(
                BaseInput::new("session", "/workspace"),
                event,
            ));
            assert_eq!(wire["hook_event_name"], expected);
            assert!(wire.as_object().unwrap().len() > 4);
        }
    }
}
