use super::*;
use std::sync::{Arc, Mutex};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
};
use tokio_util::sync::CancellationToken;
use yourai_core::context::DiscardSink;

struct Server {
    url: String,
    requests: Arc<Mutex<Vec<String>>>,
    task: tokio::task::JoinHandle<()>,
}
impl Drop for Server {
    fn drop(&mut self) {
        self.task.abort();
    }
}
async fn server(reply: Vec<u8>, keep_open: bool) -> Server {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/", listener.local_addr().unwrap());
    let requests = Arc::new(Mutex::new(vec![]));
    let seen = requests.clone();
    let task = tokio::spawn(async move {
        loop {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0; 4096];
            loop {
                let n = socket.read(&mut chunk).await.unwrap();
                if n == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..n]);
                let raw = String::from_utf8_lossy(&request);
                if let Some((headers, body)) = raw.split_once("\r\n\r\n") {
                    let length = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .map(|s| s.trim().parse::<usize>().unwrap())
                        })
                        .unwrap_or(0);
                    if body.len() >= length {
                        break;
                    }
                }
            }
            seen.lock()
                .unwrap()
                .push(String::from_utf8_lossy(&request).into());
            let _ = socket.write_all(&reply).await;
            if keep_open {
                tokio::time::sleep(Duration::from_secs(60)).await;
            }
        }
    });
    Server {
        url,
        requests,
        task,
    }
}
fn reply(mime: &str, body: &str) -> Vec<u8> {
    format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}", body.len()).into_bytes()
}
async fn call(
    tool: &dyn ToolHandler,
    input: Value,
    cancel: &CancellationToken,
) -> Result<Value, YourAiError> {
    tool.execute(
        ToolContext {
            call_id: "test".into(),
            emit: &DiscardSink,
            cancel,
            security: None,
            sandbox: None,
            interaction: None,
        },
        input,
    )
    .await
}
#[tokio::test]
async fn fetch_formats_preserve_links_and_code_and_strip_noncontent() {
    let html = "<html><head><style>STYLE_SECRET</style></head><body><h1>Guide</h1><p>See <a href='/docs'>docs</a> &amp; examples.</p><pre><code>let x = 1;</code></pre><script>SCRIPT_SECRET</script></body></html>";
    let server = server(reply("text/html; charset=utf-8", html), false).await;
    let tool = WebFetch::new().unwrap();
    let cancel = CancellationToken::new();
    let value = call(&tool, json!({"url":server.url}), &cancel)
        .await
        .unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(
        content.contains("Guide")
            && content.contains("[docs](/docs)")
            && content.contains("let x = 1;")
    );
    assert!(!content.contains("SCRIPT_SECRET") && !content.contains("STYLE_SECRET"));
    let value = call(&tool, json!({"url":server.url,"format":"text"}), &cancel)
        .await
        .unwrap();
    let content = value["content"].as_str().unwrap();
    assert!(content.contains("See docs & examples."));
    assert!(!content.contains("<p>") && !content.contains("[docs]"));
    let value = call(&tool, json!({"url":server.url,"format":"html"}), &cancel)
        .await
        .unwrap();
    assert_eq!(value["content"], html);
    assert_eq!(server.requests.lock().unwrap().len(), 3);
}
#[tokio::test]
async fn fetch_rejects_invalid_urls_binary_status_and_oversized_chunked_bodies() {
    let tool = WebFetch::new().unwrap();
    let cancel = CancellationToken::new();
    for url in [
        "file:///etc/passwd",
        "https://name:secret@example.com",
        "ftp://example.com",
    ] {
        assert!(call(&tool, json!({"url":url}), &cancel).await.is_err());
    }
    for (response, expected) in [
        (
            reply("application/pdf", "%PDF-1.7"),
            "unsupported content type",
        ),
        (
            b"HTTP/1.1 429 Too Many Requests\r\nContent-Length: 0\r\n\r\n".to_vec(),
            "HTTP 429",
        ),
        (
            b"HTTP/1.1 200 OK\r\nContent-Length: 9999999\r\n\r\n".to_vec(),
            "exceeds 5 MiB",
        ),
        (
            format!(
                "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
                MAX_RESPONSE_BYTES + 1,
                "x".repeat(MAX_RESPONSE_BYTES + 1)
            )
            .into_bytes(),
            "exceeds 5 MiB",
        ),
    ] {
        let server = server(response, false).await;
        let err = call(&tool, json!({"url":server.url}), &cancel)
            .await
            .unwrap_err();
        assert!(err.to_string().contains(expected), "{err}");
        assert_eq!(server.requests.lock().unwrap().len(), 1);
    }
}
#[tokio::test]
async fn redirect_is_bounded_and_returned_url_is_the_final_page() {
    let target = server(reply("text/plain", "redirected"), false).await;
    let redirect = server(
        format!(
            "HTTP/1.1 302 Found\r\nLocation: {}\r\nContent-Length: 0\r\n\r\n",
            target.url
        )
        .into_bytes(),
        false,
    )
    .await;
    let tool = WebFetch::new().unwrap();
    let cancel = CancellationToken::new();
    let value = call(&tool, json!({"url":redirect.url}), &cancel)
        .await
        .unwrap();
    assert_eq!(value["url"], target.url);
    assert_eq!(value["content"], "redirected");
    let cycle = server(
        b"HTTP/1.1 302 Found\r\nLocation: /\r\nContent-Length: 0\r\n\r\n".to_vec(),
        false,
    )
    .await;
    assert!(call(&tool, json!({"url":cycle.url}), &cancel)
        .await
        .is_err());
    assert_eq!(cycle.requests.lock().unwrap().len(), 5);
}
#[tokio::test]
async fn cancellation_and_timeout_cover_body_reads() {
    let tool = WebFetch::new().unwrap();
    let pending =
        b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 100\r\n\r\n".to_vec();
    let server = server(pending.clone(), true).await;
    let cancel = CancellationToken::new();
    let work = call(&tool, json!({"url":server.url}), &cancel);
    let (result, _) = tokio::join!(work, async {
        tokio::time::sleep(Duration::from_millis(30)).await;
        cancel.cancel();
    });
    assert!(matches!(
        result,
        Err(YourAiError::Aborted(AbortReason::Cancelled))
    ));
    let server = self::server(pending, true).await;
    let cancel = CancellationToken::new();
    let err = call(&tool, json!({"url":server.url,"timeout":1}), &cancel)
        .await
        .unwrap_err();
    assert!(err.to_string().contains("timed out"));
}
#[tokio::test]
async fn search_parses_json_and_sse_without_waiting_for_connection_close() {
    let body = json!({"jsonrpc":"2.0","id":1,"result":{"content":[{"type":"text","text":"Title: Docs\nURL: https://example.com"},{"type":"text","text":"Excerpt"}]}}).to_string();
    for (response, keep_open) in [
        (reply("application/json", &body), false),
        (format!("HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: close\r\n\r\n: ping\n\nevent: message\ndata:{body}\n\n").into_bytes(), true),
    ] {
        let server = server(response, keep_open).await;
        let tool = WebSearch { client: client(false).unwrap(), endpoint: Url::parse(&server.url).unwrap() };
        let cancel = CancellationToken::new();
        let value = tokio::time::timeout(Duration::from_secs(2), call(&tool, json!({"query":"rust documentation","num_results":3}), &cancel)).await.unwrap().unwrap();
        assert!(value["content"].as_str().unwrap().ends_with("Excerpt"));
        let requests = server.requests.lock().unwrap();
        assert_eq!(requests.len(), 1);
        let body: Value = serde_json::from_str(requests[0].split_once("\r\n\r\n").unwrap().1).unwrap();
        assert_eq!(body["params"]["name"], "web_search_exa");
        assert_eq!(body["params"]["arguments"]["numResults"], 3);
        assert!(body["params"]["arguments"].get("session_id").is_none());
    }
}
#[test]
fn search_protocol_errors_and_partial_frames_are_not_successes() {
    assert!(search_reply(r#"{"id":1,"error":{"code":-1}}"#).is_err());
    assert!(search_reply(r#"{"id":1,"result":{"isError":true,"content":[]}}"#).is_err());
    assert!(
        search_reply("data: {\"id\":1,\"result\":{\"content\":[]}}\n")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        search_reply("data: {\"id\":1,\ndata: \"result\":{\"content\":[]}}\n\n").unwrap(),
        Some(String::new())
    );
}
#[tokio::test]
async fn invalid_inputs_and_precancelled_calls_never_send_and_require_network_approval() {
    let server = server(reply("text/plain", "unused"), false).await;
    let fetch = WebFetch::new().unwrap();
    let search = WebSearch {
        client: client(false).unwrap(),
        endpoint: Url::parse(&server.url).unwrap(),
    };
    let cancel = CancellationToken::new();
    assert!(call(&search, json!({"query":" "}), &cancel).await.is_err());
    assert!(
        call(&search, json!({"query":"x","num_results":21}), &cancel)
            .await
            .is_err()
    );
    assert!(call(&fetch, json!({"url":server.url,"timeout":0}), &cancel)
        .await
        .is_err());
    cancel.cancel();
    assert!(matches!(
        call(&fetch, json!({"url":server.url}), &cancel).await,
        Err(YourAiError::Aborted(_))
    ));
    assert!(matches!(
        call(&search, json!({"query":"hello"}), &cancel).await,
        Err(YourAiError::Aborted(_))
    ));
    assert!(server.requests.lock().unwrap().is_empty());
    for tool in [&fetch as &dyn ToolHandler, &search] {
        let context = tool.security_context(&json!({}));
        assert!(context.is_network && !context.is_destructive);
    }
}

#[tokio::test]
#[ignore = "requires public internet; no API key or user data is sent"]
async fn live_public_web_tools() {
    let cancel = CancellationToken::new();
    let fetch = WebFetch::new().unwrap();
    let result = call(
        &fetch,
        json!({"url":"https://www.rust-lang.org/","format":"markdown"}),
        &cancel,
    )
    .await
    .unwrap();
    assert!(result["content"].as_str().unwrap().contains("Rust"));
    let search = WebSearch {
        client: client(false).unwrap(),
        endpoint: Url::parse(SEARCH_ENDPOINT).unwrap(),
    };
    let result = call(
        &search,
        json!({"query":"Rust official documentation ownership","num_results":1}),
        &cancel,
    )
    .await
    .unwrap();
    let content = result["content"].as_str().unwrap();
    assert!(content.contains("https://"));
    println!("Public webfetch and anonymous Exa websearch passed");
}
