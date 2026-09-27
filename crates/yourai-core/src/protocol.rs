//! Messages shared by frontends and agent execution.
//! Cancellation uses the separate control channel; Ask/Reply carries interactions.
//! Tool-specific payloads stay in JSON values rather than dedicated variants.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// region:    --- In ---

/// 外界 → loop 的消息（turn 作用域，走 inbox，loop 独占拉取消费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum In {
    /// 对话输入 & steer（同一条路：首条 = 用户输入，后续 = mid-turn 注入）
    UserText {
        text: String,
        /// 仅影响运行中追加输入；作为首条输入时直接开始本次 Turn。
        /// 旧 wire 消息缺省为 Steer。
        #[serde(default)]
        mode: InputMode,
    },

    /// 对一切 [`Out::Ask`] 的答复（审批/提问/表单/计划确认/MCP elicitation）
    Reply { id: String, payload: Value },
}

impl In {
    /// 便捷构造：用户输入
    pub fn user_text(text: impl Into<String>) -> Self {
        In::UserText {
            text: text.into(),
            mode: InputMode::Steer,
        }
    }

    pub fn follow_up(text: impl Into<String>) -> Self {
        In::UserText {
            text: text.into(),
            mode: InputMode::FollowUp,
        }
    }
}

/// 用户输入的消费时机，取消仍独立走控制通道。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum InputMode {
    #[default]
    Steer,
    FollowUp,
}

// endregion: --- In ---

// region:    --- Out ---

/// loop → 外界的消息（turn 作用域，走 outbox，同步 fire-and-forget）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Out {
    /// 正文流式增量
    Chunk { text: String },

    /// 思考流式增量（thinking / R1 类模型的 reasoning）
    Reasoning { text: String },

    /// 完整消息
    Message { text: String },

    /// 工具调用开始
    ToolStarted {
        id: String,
        name: String,
        input: Value,
    },

    /// 长工具增量通道：shell stdout 逐行 / browser 截图 / subagent 事件树转发
    ToolProgress { id: String, payload: Value },

    /// 工具调用结束
    ToolDone {
        id: String,
        name: String,
        output: Value,
        is_error: bool,
    },

    /// 一切"loop 问外界"（审批/提问/表单/计划确认/MCP elicitation）
    Ask { id: String, payload: Value },

    /// 模型请求失败后的自动重试预告（含重试序号、上限与等待毫秒）。
    Retry {
        attempt: u32,
        max: u32,
        reason: String,
        wait_ms: u64,
    },

    /// token 用量
    Usage { usage: Usage },

    /// 提示级通知：压缩、降级、非致命错误
    Notice { level: Level, message: String },
}

// endregion: --- Out ---

// region:    --- 支撑类型 ---

/// [`Out::Notice`] 的级别
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[non_exhaustive]
pub enum Level {
    Info,
    Warning,
    Error,
}

/// token 用量（协议自有类型，保持叶子零依赖；loop 负责从 LLM 库类型转换）
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub total_tokens: u64,
}

// endregion: --- 支撑类型 ---
