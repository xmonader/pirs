use std::sync::Arc;

use anyhow::Context as _;
use pirs_agent::{Agent, AgentEvent};
use pirs_ai::Message;
use serde_json::json;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpListener;
use tokio::sync::broadcast;

const PAGE: &str = include_str!("../assets/serve.html");

pub struct ServeOptions {
    pub agent: Agent,
    #[allow(dead_code)]
    pub host: Option<Arc<pirs_rhai::ExtensionHost>>,
    pub port: u16,
    pub bind: String,
    pub token: String,
    pub allow_external: bool,
}

pub async fn run(mut opts: ServeOptions) -> anyhow::Result<()> {
    let (tx, _) = broadcast::channel::<String>(512);
    {
        let tx = tx.clone();
        opts.agent.subscribe(Arc::new(move |event: AgentEvent| {
            let v = match &event {
                AgentEvent::MessageStart { message } => {
                    json!({"type": "message_start", "role": role_of(message)})
                }
                AgentEvent::MessageUpdate { message } => {
                    json!({"type": "message_update", "text": message.text(), "thinking": thinking_of(message)})
                }
                AgentEvent::MessageEnd { message } => {
                    json!({"type": "message_end", "role": role_of(message), "text": text_of(message)})
                }
                AgentEvent::ToolExecutionStart { tool_name, args, .. } => {
                    json!({"type": "tool_start", "name": tool_name, "args": args})
                }
                AgentEvent::ToolExecutionEnd { result, .. } => {
                    let text: String = result.content.iter().filter_map(|b| b.as_text()).collect::<Vec<_>>().join("\n");
                    json!({"type": "tool_end", "name": result.tool_name, "isError": result.is_error, "text": text})
                }
                AgentEvent::CompactionStart { .. } => json!({"type": "status", "text": "compacting..."}),
                AgentEvent::CompactionEnd { .. } => json!({"type": "status", "text": "compaction done"}),
                _ => return,
            };
            let _ = tx.send(v.to_string());
        }));
    }

    let agent = Arc::new(tokio::sync::Mutex::new(opts.agent));
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let agent = Arc::clone(&agent);
        let tx = tx.clone();
        tokio::spawn(async move {
            while let Some(text) = prompt_rx.recv().await {
                let running = {
                    let a = agent.lock().await;
                    if a.is_running() {
                        a.steer(Message::user(text.clone()));
                        true
                    } else {
                        false
                    }
                };
                if running {
                    let _ = tx.send(json!({"type": "status", "text": "steered"}).to_string());
                    continue;
                }
                let _ = tx
                    .send(json!({"type": "message_end", "role": "user", "text": text}).to_string());
                let mut a = agent.lock().await;
                let _ = a.prompt(text).await;
                let report = a.usage_report();
                let total = report.grand_total();
                let _ = tx.send(
                    json!({
                        "type": "usage",
                        "input": total.input,
                        "cacheRead": total.cache_read,
                        "output": total.output,
                        "calls": report.calls.len(),
                    })
                    .to_string(),
                );
            }
        });
    }

    if !matches!(opts.bind.as_str(), "127.0.0.1" | "localhost" | "::1") && !opts.allow_external {
        anyhow::bail!(
            "refusing to bind {} without --serve-external (and set --serve-token for auth)",
            opts.bind
        );
    }
    let addr = format!("{}:{}", opts.bind, opts.port);
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    eprintln!("[pirs serve: http://{addr} (token auth required for writes)]");

    loop {
        let (stream, _) = listener.accept().await?;
        let state = AppState {
            events: tx.clone(),
            prompts: prompt_tx.clone(),
            agent: Arc::clone(&agent),
            token: opts.token.clone(),
        };
        tokio::spawn(async move {
            if let Err(e) = handle_connection(stream, state).await {
                tracing::debug!("connection closed: {e}");
            }
        });
    }
}

#[derive(Clone)]
struct AppState {
    events: broadcast::Sender<String>,
    prompts: tokio::sync::mpsc::UnboundedSender<String>,
    agent: Arc<tokio::sync::Mutex<Agent>>,
    token: String,
}

async fn handle_connection(stream: tokio::net::TcpStream, state: AppState) -> anyhow::Result<()> {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut request_line = String::new();
    reader.read_line(&mut request_line).await?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().unwrap_or("");
    let path = parts.next().unwrap_or("/");

    let mut content_length = 0usize;
    let mut authorization = String::new();
    let mut origin = String::new();
    let mut header_bytes = 0usize;
    loop {
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        header_bytes += line.len();
        if header_bytes > 16 * 1024 {
            respond(&mut write, 431, "text/plain", "headers too large").await?;
            return Ok(());
        }
        if line.trim().is_empty() {
            break;
        }
        let lower = line.to_ascii_lowercase();
        if let Some(rest) = lower.strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
            if content_length > 8 * 1024 * 1024 {
                respond(&mut write, 413, "text/plain", "body too large").await?;
                return Ok(());
            }
        }
        if let Some(rest) = lower.strip_prefix("authorization:") {
            authorization = rest.trim().to_string();
        }
        if let Some(rest) = lower.strip_prefix("origin:") {
            origin = rest.trim().to_string();
        }
    }

    let is_write = matches!(method, "POST");
    if is_write {
        if !origin.is_empty()
            && !origin.contains("localhost")
            && !origin.contains("127.0.0.1")
            && !origin.contains("::1")
        {
            respond(
                &mut write,
                403,
                "text/plain",
                "cross-origin writes rejected",
            )
            .await?;
            return Ok(());
        }
        let expected = format!("Bearer {}", state.token);
        if !constant_time_eq(authorization.as_bytes(), expected.as_bytes()) {
            respond(
                &mut write,
                403,
                "text/plain",
                "missing or invalid bearer token",
            )
            .await?;
            return Ok(());
        }
    }

    match (method, path) {
        ("GET", "/") | ("GET", "/index.html") => {
            let page = PAGE.replace("__PIRS_TOKEN__", &state.token);
            respond(&mut write, 200, "text/html; charset=utf-8", &page).await?;
        }
        ("GET", "/events") => {
            write
                .write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncache-control: no-cache\r\nconnection: close\r\n\r\n")
                .await?;
            let mut rx = state.events.subscribe();
            loop {
                match rx.recv().await {
                    Ok(data) => {
                        let frame = format!("data: {data}\n\n");
                        if write.write_all(frame.as_bytes()).await.is_err() {
                            break;
                        }
                        let _ = write.flush().await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(_) => break,
                }
            }
        }
        ("POST", "/prompt") => {
            let mut body = vec![0u8; content_length];
            reader.read_exact(&mut body).await?;
            let v: serde_json::Value = serde_json::from_slice(&body).unwrap_or(json!({}));
            let text = v.get("text").and_then(|t| t.as_str()).unwrap_or("");
            if !text.trim().is_empty() {
                let _ = state.prompts.send(text.to_string());
            }
            respond(&mut write, 200, "application/json", "{\"ok\":true}").await?;
        }
        ("POST", "/cancel") => {
            {
                let a = state.agent.lock().await;
                a.cancel();
            }
            respond(&mut write, 200, "application/json", "{\"ok\":true}").await?;
        }
        ("GET", "/state") => {
            let body = {
                let a = state.agent.lock().await;
                let report = a.usage_report();
                let total = report.grand_total();
                json!({
                    "model": a.model,
                    "running": a.is_running(),
                    "usage": {
                        "input": total.input,
                        "cacheRead": total.cache_read,
                        "output": total.output,
                        "calls": report.calls.len(),
                    }
                })
            };
            respond(&mut write, 200, "application/json", &body.to_string()).await?;
        }
        _ => {
            respond(&mut write, 404, "text/plain", "not found").await?;
        }
    }
    Ok(())
}

async fn respond(
    write: &mut tokio::net::tcp::OwnedWriteHalf,
    status: u16,
    content_type: &str,
    body: &str,
) -> anyhow::Result<()> {
    let status_text = match status {
        200 => "200 OK",
        404 => "404 Not Found",
        _ => "500 Internal Server Error",
    };
    write
        .write_all(
            format!(
                "HTTP/1.1 {status_text}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{}",
                body.len(),
                body
            )
            .as_bytes(),
        )
        .await?;
    write.flush().await?;
    Ok(())
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn role_of(m: &Message) -> &'static str {
    match m {
        Message::User(_) => "user",
        Message::Assistant(_) => "assistant",
        Message::ToolResult(_) => "toolResult",
    }
}

fn text_of(m: &Message) -> String {
    match m {
        Message::User(u) => match &u.content {
            pirs_ai::UserContent::Text(t) => t.clone(),
            pirs_ai::UserContent::Blocks(b) => b
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n"),
        },
        Message::Assistant(a) => a.text(),
        Message::ToolResult(t) => t
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n"),
    }
}

fn thinking_of(a: &pirs_ai::AssistantMessage) -> String {
    a.content
        .iter()
        .filter_map(|b| match b {
            pirs_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect()
}
