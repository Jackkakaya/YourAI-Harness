//! 两级错误（决策 5.7）。
//!
//! 第一级按**性质**二分：[`YourAiError::Aborted`]（终止，不是故障）与
//! [`YourAiError::Error`]（一切故障）。消费方只看第一级；
//! 来源细分在第二级 [`ErrorKind`]。

/// 框架统一错误。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum YourAiError {
    /// 终止——不是故障（ESC、消费端消失、关停）。前端渲染为灰色"已停止"。
    #[error("turn aborted: {0}")]
    Aborted(#[from] AbortReason),

    /// 一切故障的统一入口。前端渲染为红色错误。
    #[error("{0}")]
    Error(#[from] ErrorKind),
}

impl YourAiError {
    /// Some SDK paths preserve headers (non-streaming); streaming HttpError currently does not.
    pub fn model_http_header(&self, name: &str) -> Option<&str> {
        let Self::Error(ErrorKind::Model { source }) = self else {
            return None;
        };
        let mut current = source;
        for _ in 0..16 {
            match current {
                genai::Error::WebModelCall {
                    webc_error: genai::webc::Error::ResponseFailedStatus { headers, .. },
                    ..
                } => return headers.get(name)?.to_str().ok(),
                genai::Error::WebStream { error, .. } => {
                    current = error.downcast_ref::<genai::Error>()?
                }
                _ => return None,
            }
        }
        None
    }
    /// HTTP status/body preserved inside the SDK's typed streaming error wrappers.
    /// Never classify errors by searching their human-readable display text.
    pub fn model_http_error(&self) -> Option<(u16, &str)> {
        let Self::Error(ErrorKind::Model { source }) = self else {
            return None;
        };
        let mut current = source;
        for _ in 0..16 {
            match current {
                genai::Error::HttpError { status, body, .. } => {
                    return Some((status.as_u16(), body.as_str()))
                }
                genai::Error::WebModelCall {
                    webc_error: genai::webc::Error::ResponseFailedStatus { status, body, .. },
                    ..
                } => return Some((status.as_u16(), body)),
                genai::Error::WebStream { error, .. } => {
                    current = error.downcast_ref::<genai::Error>()?
                }
                _ => return None,
            }
        }
        None
    }
}

/// 终止的原因。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AbortReason {
    #[error("stopped by hook: {0}")]
    HookStopped(String),
    /// cancel token 触发（ESC / Shutdown）
    #[error("cancelled")]
    Cancelled,

    /// inbox/outbox 对端消失（调用方已离开）
    #[error("client disconnected")]
    Disconnected,

    /// 总执行或当前操作的截止时间到达；调用方根据操作边界决定收尾方式。
    #[error("deadline exceeded")]
    DeadlineExceeded,
}

/// 故障来源细分（第二级）。
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ErrorKind {
    /// LLM 失败——唯一有具体类型的下沉点，原样携带 genai 错误
    #[error("model error: {source}")]
    Model {
        #[from]
        source: genai::Error,
    },

    /// 其他 provider 运行失败（统一形状，不强造子类）
    #[error("provider '{name}' failed: {message}")]
    Provider { name: &'static str, message: String },

    /// 工具执行失败（DefaultLoop 消化为 ToolDone{is_error}，不逃逸成 turn 失败）
    #[error("tool '{name}' failed: {message}")]
    Tool { name: String, message: String },

    /// 装配/配置错误：缺必需 provider、参数非法——在**使用点**报（决策 5.8）
    #[error("config error: {0}")]
    Config(String),

    /// loop 自身逻辑错误
    #[error("loop error: {0}")]
    Loop(String),

    /// 兜底
    #[error("{0}")]
    Other(String),
}

#[cfg(test)]
mod tests {
    use super::*;
    fn wrap(error: genai::Error) -> genai::Error {
        genai::Error::WebStream {
            model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm"),
            cause: error.to_string(),
            error: Box::new(error),
        }
    }
    #[test]
    fn http_status_survives_nested_stream_wrappers() {
        let source = genai::Error::HttpError {
            status: "429".parse().unwrap(),
            canonical_reason: "Too Many Requests".into(),
            body: r#"{"error":{"message":"rpm exceeded","dimension":"rpm"}}"#.into(),
        };
        let error = YourAiError::from(ErrorKind::Model {
            source: wrap(wrap(source)),
        });
        let (status, body) = error.model_http_error().unwrap();
        assert_eq!(status, 429);
        assert!(body.contains("rpm exceeded"));
    }
    #[test]
    fn display_text_is_not_a_structured_http_status() {
        let error = YourAiError::from(ErrorKind::Model {
            source: genai::Error::WebStream {
                model_iden: genai::ModelIden::new(genai::adapter::AdapterKind::OpenAI, "glm"),
                cause: "HTTP 429".into(),
                error: Box::new(std::io::Error::other("HTTP 429")),
            },
        });
        assert!(error.model_http_error().is_none());
    }
}
