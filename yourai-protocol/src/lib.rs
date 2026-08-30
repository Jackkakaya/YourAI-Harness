//! # yourai-protocol
//!
//! YourAI 的共同语言：loop 与外界之间的全部消息词汇。
//!
//! 本 crate 是**叶子 crate**（零实现依赖，仅 serde）——core / loop / 前端 / 工具
//! 全体依赖它；加变体只改这里，`yourai-core` 源码不动（`#[non_exhaustive]`
//! 保证演化不炸下游穷举 match）。
//!
//! 语义约定：
//! - 取消不走 [`In`]（控制面，走 core 的 `CancellationToken`）
//! - [`Out::Ask`] + [`In::Reply`] 是唯一的一问一答机制，审批/提问/表单/
//!   计划确认全是 payload 约定
//! - 协议不携带任何工具的专属变体：工具三段式骨架 + Value 载荷，
//!   渲染知识在前端按 tool name 注册

use serde::{Deserialize, Serialize};
use serde_json::Value;

// region:    --- In ---

/// 外界 → loop 的消息（turn 作用域，走 inbox，loop 独占拉取消费）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[non_exhaustive]
pub enum In {
    /// 对话输入 & steer（同一条路：首条 = 用户输入，后续 = mid-turn 注入）
    UserText { text: String },

    /// 对一切 [`Out::Ask`] 的答复（审批/提问/表单/计划确认/MCP elicitation）
    Reply { id: String, payload: Value },
}

impl In {
    /// 便捷构造：用户输入
    pub fn user_text(text: impl Into<String>) -> Self {
        In::UserText { text: text.into() }
    }
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
    ToolStarted { id: String, name: String, input: Value },

    /// 长工具增量通道：shell stdout 逐行 / browser 截图 / subagent 事件树转发
    ToolProgress { id: String, payload: Value },

    /// 工具调用结束
    ToolDone { id: String, name: String, output: Value, is_error: bool },

    /// 一切"loop 问外界"（审批/提问/表单/计划确认/MCP elicitation）
    Ask { id: String, payload: Value },

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
