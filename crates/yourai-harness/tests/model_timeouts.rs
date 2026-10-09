#[path = "support/loop.rs"]
mod support;
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use yourai_core::prelude::*;
use yourai_harness::GenaiModel;
use yourai_harness::{
    default_loop::{DefaultLoop, LoopConfig},
    MeteredModel, ModelBudget,
};

async fn server(mode: &'static str) -> (GenaiModel, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = genai::resolver::Endpoint::from_owned(format!(
        "http://{}/v1/",
        listener.local_addr().unwrap()
    ));
    let task = tokio::spawn(async move {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut request = Vec::new();
        let mut buffer = [0; 4096];
        loop {
            let len = socket.read(&mut buffer).await.unwrap();
            assert!(len > 0);
            request.extend_from_slice(&buffer[..len]);
            if let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") {
                let headers = String::from_utf8_lossy(&request[..header_end]);
                let length: usize = headers
                    .lines()
                    .find_map(|line| {
                        line.to_lowercase()
                            .strip_prefix("content-length:")
                            .map(|n| n.trim().parse().unwrap())
                    })
                    .unwrap();
                if request.len() >= header_end + 4 + length {
                    let body: serde_json::Value =
                        serde_json::from_slice(&request[header_end + 4..header_end + 4 + length])
                            .unwrap();
                    assert_eq!(body["stream"], true);
                    assert!(body.get("stream_header_timeout").is_none());
                    assert!(body.get("stream_read_timeout").is_none());
                    break;
                }
            }
        }
        if mode == "rate-limit" {
            let body = r#"{"error":{"code":"rate_limit_exceeded"}}"#;
            socket.write_all(format!("HTTP/1.1 429 Too Many Requests\r\nRetry-After: 0.25\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).as_bytes()).await.unwrap();
            return;
        }
        if mode == "headers" {
            std::future::pending::<()>().await;
        }
        if mode == "cancel-before-headers" {
            assert_eq!(socket.read(&mut buffer).await.unwrap(), 0);
            return;
        }
        if mode == "slow-headers" {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        socket
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n",
            )
            .await
            .unwrap();
        if mode == "slow-body" {
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
        if mode == "empty-body" {
            std::future::pending::<()>().await;
        }
        if mode == "heartbeat" {
            for _ in 0..6 {
                socket.write_all(b": ping\n\n").await.unwrap();
                tokio::time::sleep(Duration::from_millis(70)).await;
            }
        }
        if mode == "fragment" {
            let event = json!({"id":"test","object":"chat.completion.chunk","model":"test","choices":[{"index":0,"delta":{"role":"assistant"},"finish_reason":null}]});
            let data = format!("data: {event}\n\n");
            // No complete SSE event is available for over 200ms, yet each read progresses.
            for chunk in data.as_bytes().chunks(data.len().div_ceil(6)) {
                socket.write_all(chunk).await.unwrap();
                tokio::time::sleep(Duration::from_millis(70)).await;
            }
        }
        if mode == "heartbeat-until-cancel" {
            loop {
                tokio::select! {
                    len = socket.read(&mut buffer) => { assert_eq!(len.unwrap(), 0); return; }
                    _ = tokio::time::sleep(Duration::from_millis(30)) => {
                        if socket.write_all(b": ping\n\n").await.is_err() { return; }
                    }
                }
            }
        }
        if mode == "chunk" {
            let event = json!({"id":"test","object":"chat.completion.chunk","model":"test","choices":[{"index":0,"delta":{"content":"started"},"finish_reason":null}]});
            socket
                .write_all(format!("data: {event}\n\n").as_bytes())
                .await
                .unwrap();
            std::future::pending::<()>().await;
        }
        if mode == "cancel" {
            assert_eq!(
                socket.read(&mut buffer).await.unwrap(),
                0,
                "dropped request must close its response stream"
            );
            return;
        }
        for index in 0..6 {
            let event = json!({"id":"test","object":"chat.completion.chunk","model":"test","choices":[{"index":0,"delta":{"content":format!("part{index} ")},"finish_reason":null}]});
            socket
                .write_all(format!("data: {event}\n\n").as_bytes())
                .await
                .unwrap();
            tokio::time::sleep(Duration::from_millis(60)).await;
        }
        let end = json!({"id":"test","object":"chat.completion.chunk","model":"test","choices":[{"index":0,"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":6,"total_tokens":16}});
        socket
            .write_all(format!("data: {end}\n\ndata: [DONE]\n\n").as_bytes())
            .await
            .unwrap();
    });
    let client = genai::Client::builder()
        .with_adapter_kind(genai::adapter::AdapterKind::OpenAI)
        .with_auth_resolver_fn(|_| Ok(Some(genai::resolver::AuthData::from_single("test"))))
        .with_service_target_resolver_fn(move |mut target: genai::ServiceTarget| {
            target.endpoint = endpoint.clone();
            Ok(target)
        })
        .build()
        .unwrap();
    (
        GenaiModel::new(client, "test").with_timeouts(
            Some(Duration::from_millis(200)),
            Some(Duration::from_millis(200)),
        ),
        task,
    )
}

#[tokio::test]
async fn collected_response_bounds_headers_and_idle_chunks_without_total_deadline() {
    for mode in ["headers", "chunk", "empty-body", "progress"] {
        let (model, task) = server(mode).await;
        let result = tokio::time::timeout(
            Duration::from_secs(3),
            model.complete(
                ModelRequest::new(
                    ChatRequest::new(vec![ChatMessage::user("summarize")]),
                    ChatOptions::default(),
                )
                .with_context("compact", "test"),
            ),
        )
        .await
        .unwrap();
        if mode == "progress" {
            let response = result.unwrap();
            assert_eq!(
                response.content.first_text(),
                Some("part0 part1 part2 part3 part4 part5 ")
            );
            assert_eq!(response.usage.total_tokens, Some(16));
            assert!(response.stop_reason.is_some());
            task.await.unwrap();
        } else {
            let error = result.unwrap_err().to_string();
            assert!(
                error.contains(if mode == "headers" {
                    "response headers timed out"
                } else {
                    "body read timed out"
                }),
                "{error}"
            );
            task.abort();
            let _ = task.await;
        }
    }
}

#[tokio::test]
async fn cancelling_collected_response_drops_the_network_request() {
    for mode in ["cancel", "cancel-before-headers"] {
        let (model, server) = server(mode).await;
        let request = ModelRequest::new(
            ChatRequest::new(vec![ChatMessage::user("summarize")]),
            ChatOptions::default(),
        );
        assert!(
            tokio::time::timeout(Duration::from_millis(100), model.complete(request))
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(1), server)
            .await
            .unwrap()
            .unwrap();
    }
}

fn request() -> ModelRequest {
    ModelRequest::new(
        ChatRequest::new(vec![ChatMessage::user("summarize")]),
        ChatOptions::default(),
    )
    .with_context("compact", "test")
}

#[tokio::test]
async fn headers_and_body_have_independent_deadlines() {
    for mode in ["slow-headers", "slow-body"] {
        let (model, server) = server(mode).await;
        let (header, read) = if mode == "slow-headers" {
            (700, 200)
        } else {
            (200, 700)
        };
        let model = model.with_timeouts(
            Some(Duration::from_millis(header)),
            Some(Duration::from_millis(read)),
        );
        let response = tokio::time::timeout(Duration::from_secs(3), model.complete(request()))
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            response.content.first_text(),
            Some("part0 part1 part2 part3 part4 part5 ")
        );
        server.await.unwrap();
    }
}

#[tokio::test]
async fn raw_byte_progress_keeps_compaction_and_metered_main_loop_alive() {
    for mode in ["heartbeat", "fragment"] {
        for main in [false, true] {
            let (model, server) = server(mode).await;
            let model = Arc::new(MeteredModel {
                inner: Arc::new(model),
                budget: ModelBudget::new(),
            });
            let text = if main {
                let agent = Agent::builder()
                    .model(model)
                    .context_manager(Arc::new(support::History::default()))
                    .agent_loop(Arc::new(DefaultLoop::new(LoopConfig {
                        execution: yourai_core::execution::ExecutionConfig {
                            max_model_retries: 0,
                            ..Default::default()
                        },
                        ..Default::default()
                    })))
                    .build();
                tokio::time::timeout(Duration::from_secs(3), agent.run(In::user_text("hello")))
                    .await
                    .unwrap()
                    .unwrap()
                    .text
            } else {
                tokio::time::timeout(Duration::from_secs(3), model.complete(request()))
                    .await
                    .unwrap()
                    .unwrap()
                    .content
                    .first_text()
                    .unwrap()
                    .to_owned()
            };
            assert_eq!(text, "part0 part1 part2 part3 part4 part5 ");
            server.await.unwrap();
        }
    }
}

#[tokio::test]
async fn explicit_turn_deadline_still_stops_a_healthy_heartbeat_stream() {
    let (model, server) = server("heartbeat-until-cancel").await;
    let agent = Agent::builder()
        .model(Arc::new(model))
        .context_manager(Arc::new(support::History::default()))
        .agent_loop(Arc::new(DefaultLoop::default()))
        .build();
    let mut options = TurnOptions::default();
    options.limits.deadline = Some(std::time::Instant::now() + Duration::from_millis(200));
    let (_, result) = tokio::time::timeout(
        Duration::from_secs(2),
        support::collect(agent.start_with(In::user_text("hello"), options).unwrap()),
    )
    .await
    .unwrap();
    assert!(matches!(
        *result.unwrap_err().error,
        YourAiError::Aborted(AbortReason::DeadlineExceeded)
    ));
    tokio::time::timeout(Duration::from_secs(1), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn per_turn_model_limit_is_forwarded_to_transport() {
    let (model, server) = server("slow-headers").await;
    let agent = Agent::builder()
        .model(Arc::new(MeteredModel {
            inner: Arc::new(model),
            budget: ModelBudget::new(),
        }))
        .context_manager(Arc::new(support::History::default()))
        .agent_loop(Arc::new(DefaultLoop::new(LoopConfig {
            execution: yourai_core::execution::ExecutionConfig {
                max_model_retries: 0,
                ..Default::default()
            },
            ..Default::default()
        })))
        .build();
    let mut options = TurnOptions::default();
    options.limits.model_timeout = Some(Duration::from_millis(50));
    let (_, result) = tokio::time::timeout(
        Duration::from_secs(2),
        support::collect(agent.start_with(In::user_text("hello"), options).unwrap()),
    )
    .await
    .unwrap();
    assert!(result
        .unwrap_err()
        .to_string()
        .contains("response headers timed out"));
    server.abort();
    let _ = server.await;
}

#[tokio::test]
async fn real_stream_errors_preserve_status_headers_and_retry_hint() {
    use futures_util::StreamExt;
    for collected in [false, true] {
        let (model, task) = server("rate-limit").await;
        let request = ModelRequest::new(ChatRequest::from_user("test"), ChatOptions::default());
        let error = if collected {
            model.complete(request).await.unwrap_err()
        } else {
            let mut stream = model.stream_events(request).await.unwrap();
            loop {
                if let Err(e) = stream.next().await.expect("HTTP error") {
                    break e;
                }
            }
        };
        assert_eq!(
            yourai_harness::model::failure::http_error(&error)
                .unwrap()
                .0,
            429
        );
        assert!(yourai_harness::model::failure::has_http_headers(&error));
        assert_eq!(
            yourai_harness::model::failure::http_header(&error, "retry-after"),
            Some("0.25")
        );
        assert_eq!(model.retry_after(&error), Some(Duration::from_millis(250)));
        assert_eq!(model.recovery(&error), ModelRecovery::Retry);
        task.await.unwrap();
    }
}

#[tokio::test]
async fn configured_model_defaults_reach_collected_and_default_loop_requests() {
    for collected in [false, true] {
        let (model, task) = server("headers").await;
        let model = yourai_harness::model::ConfiguredModel::new(
            Arc::new(model),
            ModelTokenBudget::default(),
            ModelTimeouts {
                headers: Duration::from_millis(40),
                read: Duration::from_millis(70),
            },
        )
        .unwrap();
        let model = Arc::new(MeteredModel {
            inner: model,
            budget: ModelBudget::new(),
        });
        assert_eq!(model.timeouts().headers, Duration::from_millis(40));
        let work = async {
            if collected {
                model
                    .complete(ModelRequest::new(
                        ChatRequest::from_user("test"),
                        ChatOptions::default(),
                    ))
                    .await
                    .unwrap_err()
            } else {
                let agent = Agent::builder()
                    .model(model)
                    .context_manager(Arc::new(support::History::default()))
                    .agent_loop(Arc::new(DefaultLoop::default()))
                    .build();
                *agent.run(In::user_text("test")).await.unwrap_err().error
            }
        };
        let error = tokio::time::timeout(Duration::from_millis(160), work)
            .await
            .expect("must inherit 40ms model timeout instead of 200/300s defaults");
        assert!(error.to_string().contains("headers timed out"), "{error}");
        task.abort();
    }
}
