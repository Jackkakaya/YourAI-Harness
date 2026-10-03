//! Regression coverage for OpenAI-compatible stream termination and recovery.
//! Local HTTP only: no provider credentials and no file-writing tool is executed.
#[path = "support/loop.rs"]
mod support;
use std::{sync::Arc, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use yourai_core::prelude::*;
use yourai_harness::{
    default_loop::{DefaultLoop, LoopConfig},
    GenaiModel,
};

async fn server(parts: Vec<Vec<u8>>) -> (GenaiModel, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = genai::resolver::Endpoint::from_owned(format!(
        "http://{}/v1/",
        listener.local_addr().unwrap()
    ));
    let server = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buf = [0; 8192];
        loop {
            let n = socket.read(&mut buf).await.unwrap();
            assert!(n > 0);
            request.extend_from_slice(&buf[..n]);
            if let Some(end) = request.windows(4).position(|b| b == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&request[..end]);
                let len: usize = head
                    .lines()
                    .find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix("content-length:")
                            .map(|s| s.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= end + 4 + len {
                    break;
                }
            }
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        for part in parts {
            socket.write_all(&part).await.unwrap();
            tokio::time::sleep(Duration::from_millis(2)).await;
        }
        socket.shutdown().await.unwrap();
    });
    let client = genai::Client::builder()
        .with_adapter_kind(genai::adapter::AdapterKind::OpenAI)
        .with_auth_resolver_fn(|_| Ok(Some(genai::resolver::AuthData::from_single("test"))))
        .with_service_target_resolver_fn(move |mut target: genai::ServiceTarget| {
            target.endpoint = endpoint.clone();
            Ok(target)
        })
        .build();
    (GenaiModel::new(client, "test"), server)
}
async fn run(parts: Vec<Vec<u8>>) -> Result<String, String> {
    let (model, server) = server(parts).await;
    let agent = Agent::builder()
        .model(Arc::new(model))
        .context_manager(Arc::new(support::History::default()))
        .agent_loop(Arc::new(DefaultLoop::new(LoopConfig {
            max_model_retries: 0,
            ..Default::default()
        })))
        .build();
    let result = tokio::time::timeout(Duration::from_secs(5), agent.run(In::user_text("test")))
        .await
        .unwrap();
    server.await.unwrap();
    result.map(|r| r.text).map_err(|e| e.to_string())
}
fn delta() -> String {
    "data: {\"choices\":[{\"index\":0,\"delta\":{\"content\":\"你好\"},\"finish_reason\":null}]}\n\n".into()
}
fn finish(reason: &str) -> String {
    format!(
        "data: {{\"choices\":[{{\"index\":0,\"delta\":{{}},\"finish_reason\":\"{reason}\"}}]}}\n\n"
    )
}
#[tokio::test]
async fn normal_done_survives_each_byte_boundary_and_unterminated_last_line() {
    for ending in ["data: [DONE]\n\n", "data: [DONE]", "data: [DONE]\r\n\r\n"] {
        let body = format!("{}{}{ending}", delta(), finish("stop"));
        // One byte at a time also splits multi-byte UTF-8 and CRLF boundaries.
        let parts = body.as_bytes().chunks(1).map(|s| s.to_vec()).collect();
        assert_eq!(run(parts).await.unwrap(), "你好");
    }
}
#[tokio::test]
async fn complete_finish_reason_without_done_is_accepted() {
    let body = format!("{}{}", delta(), finish("stop"));
    assert_eq!(run(vec![body.into_bytes()]).await.unwrap(), "你好");
}
#[tokio::test]
async fn missing_done_preserves_max_tokens_finish_reason() {
    for done in [false, true] {
        let body = format!(
            "{}{}{}",
            delta(),
            finish("length"),
            if done { "data: [DONE]\n\n" } else { "" }
        );
        let err = run(vec![body.into_bytes()]).await.unwrap_err();
        assert!(
            err.contains("model response truncated or filtered; tools will not execute"),
            "{err}"
        );
    }
}
#[tokio::test]
async fn abrupt_eof_reproduces_exact_session_error() {
    let err = run(vec![delta().into_bytes()]).await.unwrap_err();
    assert!(
        err.contains("provider 'model' failed: stream ended without terminal event"),
        "{err}"
    );
}

#[tokio::test]
async fn consumed_input_is_not_retried_by_resuming_host_after_stream_failure() {
    use tokio_util::sync::CancellationToken;
    use yourai_core::context::DiscardSink;
    use yourai_core::session_runtime::SessionContext;
    use yourai_harness::{HostConfig, SessionHost};
    let dir = tempfile::TempDir::new().unwrap();
    let history = Arc::new(support::History::default());
    let model = Arc::new(support::Model::new(vec![support::events(vec![
        support::chunk("partial"),
    ])]));
    let agent = Agent::builder()
        .model(model.clone())
        .context_manager(history.clone())
        .agent_loop(Arc::new(DefaultLoop::default()))
        .build();
    let host = SessionHost::open(
        dir.path(),
        SessionContext::new(history.id.clone(), dir.path()),
        agent,
        HostConfig::default(),
        "diagnostic",
    )
    .await
    .unwrap();
    let cancel = CancellationToken::new();
    host.submit(In::user_text("write game.js")).unwrap();
    let report = host
        .run_next(TurnLimits::default(), &DiscardSink, &cancel)
        .await
        .unwrap()
        .unwrap();
    assert!(report.result.is_err());
    assert_eq!(host.queued(), 0);
    assert!(host
        .run_next(TurnLimits::default(), &DiscardSink, &cancel)
        .await
        .unwrap()
        .is_none());
    assert_eq!(model.requests.lock().unwrap().len(), 1);
    host.close(Some(Duration::from_secs(1))).await.unwrap();
}

async fn complete(body: String) -> Result<ChatResponse, YourAiError> {
    let (model, server) = server(vec![body.into_bytes()]).await;
    let result = tokio::time::timeout(
        Duration::from_secs(5),
        model.complete(ModelRequest::new(
            ChatRequest::from_user("test"),
            ChatOptions::default(),
        )),
    )
    .await
    .unwrap();
    server.await.unwrap();
    result
}
fn tool_chunk(arguments: &str) -> String {
    format!(
        "data: {}\n\n",
        serde_json::json!({"choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"write","arguments":arguments}}]},"finish_reason":null}]})
    )
}
#[tokio::test]
async fn eof_after_tool_completion_preserves_arguments_and_usage_tail() {
    for done in [false, true] {
        let args = serde_json::json!({"path":"game.js","content":"const msg = '你好';"});
        let usage = "data: {\"choices\":[],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":20,\"total_tokens\":30}}\n\n";
        let body = format!(
            "{}{}{usage}{}",
            tool_chunk(&args.to_string()),
            finish("tool_calls"),
            if done { "data: [DONE]\n\n" } else { "" }
        );
        let response = complete(body).await.unwrap();
        assert!(matches!(
            response.stop_reason,
            Some(StopReason::ToolCall(_))
        ));
        let calls = response.content.tool_calls();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0].fn_arguments, args);
        assert_eq!(response.usage.total_tokens, Some(30));
    }
}
#[tokio::test]
async fn eof_without_completion_or_with_incomplete_arguments_never_returns_tools() {
    for (args, reason) in [
        ("{", "tool_calls"),
        ("{", "stop"),
        ("{}", "unknown"),
        ("{}", ""),
    ] {
        let body = format!(
            "{}{}",
            tool_chunk(args),
            if reason.is_empty() {
                String::new()
            } else {
                finish(reason)
            }
        );
        assert!(complete(body).await.is_err(), "args={args} reason={reason}");
    }
    assert!(complete(finish("tool_calls")).await.is_err());
}
#[tokio::test]
async fn eof_filter_and_length_fail_in_main_loop_even_with_incomplete_tools() {
    for reason in ["length", "content_filter"] {
        let body = format!("{}{}", tool_chunk("{"), finish(reason));
        let err = run(vec![body.into_bytes()]).await.unwrap_err();
        assert!(
            err.contains("model response truncated or filtered"),
            "{err}"
        );
    }
}
#[tokio::test]
async fn provider_error_after_finish_reason_is_not_swallowed() {
    let body = format!(
        "{}{}data: {{\"error\":{{\"message\":\"upstream failed\"}}}}\n\n",
        delta(),
        finish("stop")
    );
    assert!(complete(body)
        .await
        .unwrap_err()
        .to_string()
        .contains("upstream failed"));
}
