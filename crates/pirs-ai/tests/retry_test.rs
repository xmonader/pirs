use pirs_ai::{CompletionOptions, Context, LlmProvider, OpenAiCompat, StopReason, StreamEvent};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

async fn read_request(sock: &mut tokio::net::TcpStream) {
    let mut buf = Vec::new();
    let mut tmp = [0u8; 4096];
    loop {
        let n = sock.read(&mut tmp).await.unwrap();
        if n == 0 {
            break;
        }
        buf.extend_from_slice(&tmp[..n]);
        if let Some(pos) = find(&buf, b"\r\n\r\n") {
            let headers = String::from_utf8_lossy(&buf[..pos]).to_string();
            let len = headers
                .lines()
                .find_map(|l| {
                    l.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|v| v.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            if buf.len() >= pos + 4 + len {
                break;
            }
        }
    }
}

fn find(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn sse_response(chunks: &[&str]) -> String {
    let mut body = String::new();
    for c in chunks {
        body.push_str(&format!("data: {c}\n\n"));
    }
    body.push_str("data: [DONE]\n\n");
    format!(
        "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

async fn serve(responses: Vec<String>) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        for resp in responses {
            let (mut sock, _) = listener.accept().await.unwrap();
            read_request(&mut sock).await;
            sock.write_all(resp.as_bytes()).await.unwrap();
        }
    });
    format!("http://{addr}/v1")
}

async fn collect(
    stream: futures_util::stream::BoxStream<'static, StreamEvent>,
) -> pirs_ai::AssistantMessage {
    use futures_util::StreamExt;
    tokio::pin!(stream);
    while let Some(ev) = stream.next().await {
        if let StreamEvent::Done(msg) = ev {
            return *msg;
        }
    }
    panic!("no Done event");
}

#[tokio::test]
async fn retries_empty_completion_then_succeeds() {
    let url = serve(vec![
        sse_response(&[r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#]),
        sse_response(&[
            r#"{"choices":[{"delta":{"content":"hi"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]),
    ])
    .await;

    let provider = OpenAiCompat::new(Some(url)).with_max_retries(2);
    let msg = collect(
        provider
            .stream(
                "m",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await,
    )
    .await;
    assert_eq!(msg.text(), "hi");
    assert_eq!(msg.stop_reason, StopReason::Stop);
}

#[tokio::test]
async fn no_retry_without_budget() {
    let url = serve(vec![sse_response(&[
        r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
    ])])
    .await;

    let provider = OpenAiCompat::new(Some(url));
    let msg = collect(
        provider
            .stream(
                "m",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await,
    )
    .await;
    assert_eq!(msg.text(), "");
}

#[tokio::test]
async fn error_mid_stream_is_not_retried_after_deltas() {
    let url = serve(vec![sse_response(&[
        r#"{"choices":[{"delta":{"content":"partial"},"finish_reason":null}]}"#,
        r#"{"error":{"message":"upstream exploded"}}"#,
    ])])
    .await;

    let provider = OpenAiCompat::new(Some(url)).with_max_retries(3);
    let msg = collect(
        provider
            .stream(
                "m",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await,
    )
    .await;
    assert_eq!(msg.stop_reason, StopReason::Error);
    assert!(msg.error_message.unwrap().contains("upstream exploded"));
}

#[tokio::test]
async fn retries_http_500_with_backoff() {
    let url = serve(vec![
        "HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}"
            .to_string(),
        sse_response(&[
            r#"{"choices":[{"delta":{"content":"recovered"},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{},"finish_reason":"stop"}]}"#,
        ]),
    ])
    .await;

    let provider = OpenAiCompat::new(Some(url)).with_max_retries(1);
    let msg = collect(
        provider
            .stream(
                "m",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await,
    )
    .await;
    assert_eq!(msg.text(), "recovered");
}
