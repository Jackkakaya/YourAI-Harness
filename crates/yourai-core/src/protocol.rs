//! Messages shared by frontends and agent execution.
//! Cancellation uses the separate control channel; Ask/Reply carries interactions.
//! Tool-specific payloads stay in JSON values rather than dedicated variants.

use serde::{Deserialize, Serialize};
use serde_json::Value;

// region:    --- In ---

/// 用户消息附带的附件：内联 base64 媒体，或本地文件引用。
///
/// 两种形态由 harness 在消息入口（`accept_input`）统一解析为模型可见的
/// content part：
/// - [`AttachmentData::Base64`]：媒体（图片/PDF/音频）。图片先归一化
///   （尺寸/大小上限 + 自动缩放，对齐 opencode `image.ts`），再转 genai
///   [`crate::chat::ContentPart::Binary`]；
/// - [`AttachmentData::File`]：本地文件引用（对齐 opencode 的 FilePart：
///   `file://` URL + `#start-end` 行范围）。文本文件读取后按行窗口截断为
///   文本 part，图片走归一化，目录展开为一级列表——前端只传几十字节的
///   引用，读取与限额全部在 harness 侧完成。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UserAttachment {
    /// MIME 类型（base64 形态必填，如 `image/png`）；File 形态在解析时按
    /// 扩展名推导并覆盖，序列化时为空则省略。
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub content_type: String,
    /// 附件数据：内联 base64 或文件引用。字段名保持 `data`，旧 wire 消息
    /// 的 `"data": "<base64>"` 字符串形态仍然可解析（untagged）。
    pub data: AttachmentData,
    /// 可选的显示名/文件名。
    pub name: Option<String>,
}

/// 附件数据来源。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum AttachmentData {
    /// 标准 base64 编码（无 data-URL 前缀）——原始 wire 形态。
    Base64(String),
    /// 本地文件引用，由 harness 解析。相对路径基于会话 cwd。
    File(FileRef),
}

/// 对本地文件的引用；`lines` 为 1-based 闭区间行窗口（如 `#10-20`），
/// 仅对文本文件生效。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRef {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lines: Option<(u32, u32)>,
}

impl UserAttachment {
    /// 内联 base64 媒体附件。
    pub fn base64(
        content_type: impl Into<String>,
        data: impl Into<String>,
        name: Option<String>,
    ) -> Self {
        Self {
            content_type: content_type.into(),
            data: AttachmentData::Base64(data.into()),
            name,
        }
    }

    /// 本地文件引用；MIME 与内容在 harness 侧解析。
    pub fn file(path: impl Into<String>, lines: Option<(u32, u32)>) -> Self {
        Self {
            content_type: String::new(),
            data: AttachmentData::File(FileRef {
                path: path.into(),
                lines,
            }),
            name: None,
        }
    }
}

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
        /// 随消息附带的多媒体附件（图片/PDF 等）。旧 wire 消息缺省为空，
        /// 因此反序列化历史消息时向后兼容。
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        attachments: Vec<UserAttachment>,
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
            attachments: vec![],
        }
    }

    pub fn follow_up(text: impl Into<String>) -> Self {
        In::UserText {
            text: text.into(),
            mode: InputMode::FollowUp,
            attachments: vec![],
        }
    }

    /// 便捷构造：用户输入 + 多媒体附件（图片等）。
    pub fn user_text_with_attachments(
        text: impl Into<String>,
        attachments: Vec<UserAttachment>,
    ) -> Self {
        In::UserText {
            text: text.into(),
            mode: InputMode::Steer,
            attachments,
        }
    }
}

/// Ownership returned to the caller after input rejection. Rejected input is
/// never also queued for automatic retry; edit or explicitly resubmit it.
#[derive(Debug, Clone, Serialize, Deserialize, thiserror::Error)]
#[error("input rejected: {reason}")]
pub struct InputRejected {
    pub input: In,
    pub reason: String,
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
    /// Admission rejected this input before committing history. The same
    /// rejection is retained in TurnOutput for non-streaming consumers.
    InputRejected { rejection: InputRejected },

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
