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
    /// Extension host for slash commands (`/goal`, …) on POST /prompt.
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

    // Cancel handle lives outside the agent mutex so POST /cancel never waits
    // on an in-flight prompt (mirrors TUI cancel_handle).
    let cancel_slot = opts.agent.cancel_handle();
    let agent = Arc::new(tokio::sync::Mutex::new(opts.agent));
    let host = opts.host.clone();
    let (prompt_tx, mut prompt_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    {
        let agent = Arc::clone(&agent);
        let tx = tx.clone();
        let host = host.clone();
        tokio::spawn(async move {
            while let Some(text) = prompt_rx.recv().await {
                // Extension slash commands (e.g. /goal) — mirror TUI/REPL.
                if let Some(out) = try_extension_slash(&host, &text) {
                    let _ = tx.send(
                        json!({"type": "message_end", "role": "user", "text": text}).to_string(),
                    );
                    let _ = tx.send(
                        json!({"type": "message_end", "role": "system", "text": out}).to_string(),
                    );
                    continue;
                }
                // Short lock: either steer an in-flight run or begin_prompt and
                // release before awaiting (so cancel/steer stay responsive).
                let run = {
                    let mut a = agent.lock().await;
                    if a.is_running() {
                        a.steer(Message::user(text.clone()));
                        None
                    } else {
                        match a.begin_prompt(vec![Message::user(text.clone())]) {
                            Ok(fut) => Some(fut),
                            Err(_) => {
                                a.steer(Message::user(text.clone()));
                                None
                            }
                        }
                    }
                };
                if run.is_none() {
                    let _ = tx.send(json!({"type": "status", "text": "steered"}).to_string());
                    continue;
                }
                let _ = tx
                    .send(json!({"type": "message_end", "role": "user", "text": text}).to_string());
                if let Some(fut) = run {
                    let (full, _new, hit) = fut.await;
                    let mut a = agent.lock().await;
                    a.budget_hit = hit;
                    a.complete_run(full);
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
            }
        });
    }

    let loopback = matches!(opts.bind.as_str(), "127.0.0.1" | "localhost" | "::1");
    if !loopback && !opts.allow_external {
        anyhow::bail!(
            "refusing to bind {} without --serve-external (and set --serve-token for auth)",
            opts.bind
        );
    }
    // Embedding the token in GET / hands it to any local user who can reach the
    // loopback socket -> local RCE on a shared host. Off by default; opt in on a
    // trusted single-user machine. Otherwise the page prompts for the token
    // (printed to this terminal) and caches it in localStorage.
    let embed_token = loopback && env_flag("PIRS_SERVE_EMBED_TOKEN");
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
            cancel: Arc::clone(&cancel_slot),
            token: opts.token.clone(),
            embed_token,
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
    /// Cancel without taking the agent mutex (works mid-prompt).
    cancel: std::sync::Arc<std::sync::Mutex<tokio_util::sync::CancellationToken>>,
    token: String,
    /// Embed the auth token in the served page. Only safe when loopback-bound;
    /// on external binds the page must prompt for the token instead.
    embed_token: bool,
}

fn env_flag(name: &str) -> bool {
    std::env::var(name)
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// If `text` is a registered extension slash command, run it and return output.
/// Built-in TUI/REPL commands are not handled here (web UI is chat + ext cmds).
fn try_extension_slash(
    host: &Option<Arc<pirs_rhai::ExtensionHost>>,
    text: &str,
) -> Option<String> {
    let t = text.trim();
    if !t.starts_with('/') {
        return None;
    }
    let h = host.as_ref()?;
    let (cmd, arg) = match t.split_once(char::is_whitespace) {
        Some((c, a)) => (c, a.trim()),
        None => (t, ""),
    };
    let name = cmd.trim_start_matches('/');
    if name.is_empty() || !h.commands().iter().any(|(n, _)| n == name) {
        return None;
    }
    Some(match h.run_command(name, arg) {
        Ok(out) if !out.is_empty() => out,
        Ok(_) => format!("/{name} done"),
        Err(e) => format!("/{name}: {e}"),
    })
}

/// Compare an Authorization header value against the expected token. The
/// scheme is case-insensitive per RFC 9110; the token itself is compared in
/// constant time.
fn bearer_matches(authorization: &str, token: &str) -> bool {
    let Some((scheme, value)) = authorization.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    constant_time_eq(value.trim().as_bytes(), token.as_bytes())
}

/// Extract a query parameter from a request path (minimal parser; values are
/// percent-decoded only for the token's alphabet, which needs no decoding).
fn query_param(path: &str, key: &str) -> Option<String> {
    let query = path.split_once('?')?.1;
    for pair in query.split('&') {
        if let Some((k, v)) = pair.split_once('=') {
            if k == key {
                return Some(v.to_string());
            }
        }
    }
    None
}

async fn handle_connection<S>(stream: S, state: AppState) -> anyhow::Result<()>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let (read, mut write) = tokio::io::split(stream);
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
        // Header names are case-insensitive; values are case-sensitive. Split
        // at the first ':' and lowercase only the name (previously the whole
        // line was lowercased, which corrupted bearer tokens and made every
        // authenticated write fail).
        if let Some((name, value)) = line.split_once(':') {
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "content-length" => {
                    content_length = value.parse().unwrap_or(0);
                    if content_length > 8 * 1024 * 1024 {
                        respond(&mut write, 413, "text/plain", "body too large").await?;
                        return Ok(());
                    }
                }
                "authorization" => authorization = value.to_string(),
                "origin" => origin = value.to_ascii_lowercase(),
                _ => {}
            }
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
        if !bearer_matches(&authorization, &state.token) {
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

    let route = path.split('?').next().unwrap_or("/");
    match (method, route) {
        ("GET", "/") | ("GET", "/index.html") => {
            let embedded = if state.embed_token { &state.token } else { "" };
            let page = PAGE.replace("__PIRS_TOKEN__", embedded);
            respond(&mut write, 200, "text/html; charset=utf-8", &page).await?;
        }
        ("GET", "/events") => {
            // EventSource cannot set headers, so the token arrives as a query
            // parameter. It must still be validated.
            let ok = query_param(path, "token")
                .map(|t| constant_time_eq(t.as_bytes(), state.token.as_bytes()))
                .unwrap_or(false);
            if !ok {
                respond(&mut write, 403, "text/plain", "missing or invalid token").await?;
                return Ok(());
            }
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
                // Mid-run: steer without queueing a second full turn. Short lock
                // only — worker does not hold the mutex across prompt().await.
                let steered = {
                    let a = state.agent.lock().await;
                    if a.is_running() {
                        a.steer(Message::user(text));
                        true
                    } else {
                        false
                    }
                };
                if !steered {
                    let _ = state.prompts.send(text.to_string());
                }
            }
            respond(&mut write, 200, "application/json", "{\"ok\":true}").await?;
        }
        ("POST", "/cancel") => {
            // Do not take agent.lock — cancel_slot reaches the live run.
            state.cancel.lock().unwrap().cancel();
            respond(&mut write, 200, "application/json", "{\"ok\":true}").await?;
        }
        ("GET", "/state") => {
            if !bearer_matches(&authorization, &state.token) {
                respond(
                    &mut write,
                    403,
                    "text/plain",
                    "missing or invalid bearer token",
                )
                .await?;
                return Ok(());
            }
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

async fn respond<W: tokio::io::AsyncWrite + Unpin>(
    write: &mut W,
    status: u16,
    content_type: &str,
    body: &str,
) -> anyhow::Result<()> {
    let status_text = match status {
        200 => "200 OK",
        400 => "400 Bad Request",
        403 => "403 Forbidden",
        404 => "404 Not Found",
        413 => "413 Content Too Large",
        431 => "431 Request Header Fields Too Large",
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

fn thinking_of(a: &pirs_ai::AssistantMessage) -> String {
    a.content
        .iter()
        .filter_map(|b| match b {
            pirs_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.as_str()),
            _ => None,
        })
        .collect()
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bearer_matches_mixed_case_token() {
        // Regression: the header value must NOT be lowercased before compare —
        // mixed-case tokens used to 403 every authenticated write.
        assert!(bearer_matches("Bearer AbCdEf123", "AbCdEf123"));
        assert!(bearer_matches("bearer AbCdEf123", "AbCdEf123"));
        assert!(bearer_matches("BEARER AbCdEf123", "AbCdEf123"));
        assert!(!bearer_matches("Bearer abcdef123", "AbCdEf123"));
        assert!(!bearer_matches("Bearer AbCdEf12", "AbCdEf123"));
        assert!(!bearer_matches("AbCdEf123", "AbCdEf123"));
        assert!(!bearer_matches("", "AbCdEf123"));
    }

    #[test]
    fn serve_worker_uses_begin_prompt_not_prompt_across_lock() {
        // Structural: worker must begin_prompt + await outside the mutex, and
        // cancel must use cancel_slot (not agent.lock).
        let src = include_str!("serve.rs");
        assert!(
            src.contains("begin_prompt"),
            "serve worker must use begin_prompt so lock is not held across the turn"
        );
        assert!(
            src.contains("cancel_slot") || src.contains("cancel.lock()"),
            "cancel must not require agent mutex across the whole turn"
        );
        assert!(
            src.contains("if a.is_running()") && src.contains("a.steer("),
            "POST /prompt must steer mid-run instead of only queueing"
        );
        // Must use begin_prompt (not hold mutex across prompt().await).
        assert!(
            src.contains("begin_prompt(vec![Message::user"),
            "worker must begin_prompt then await outside the lock"
        );
    }

    #[test]
    fn query_param_extracts_token() {
        assert_eq!(
            query_param("/events?token=abc123", "token"),
            Some("abc123".to_string())
        );
        assert_eq!(
            query_param("/events?x=1&token=t", "token"),
            Some("t".into())
        );
        assert_eq!(query_param("/events", "token"), None);
        assert_eq!(query_param("/events?tok=abc", "token"), None);
    }

    struct StubProvider;

    #[async_trait::async_trait]
    impl pirs_ai::LlmProvider for StubProvider {
        async fn stream(
            &self,
            _model: &str,
            _context: &pirs_ai::Context,
            _options: &pirs_ai::CompletionOptions,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures::stream::BoxStream<'static, pirs_ai::StreamEvent> {
            Box::pin(futures::stream::empty())
        }
    }

    fn test_state(token: &str) -> (AppState, tokio::sync::mpsc::UnboundedReceiver<String>) {
        let (events, _) = tokio::sync::broadcast::channel(8);
        let (prompts, rx) = tokio::sync::mpsc::unbounded_channel();
        let agent = Agent::new(std::sync::Arc::new(StubProvider), "stub");
        (
            AppState {
                events,
                prompts,
                cancel: agent.cancel_handle(),
                agent: std::sync::Arc::new(tokio::sync::Mutex::new(agent)),
                token: token.to_string(),
                embed_token: true,
            },
            rx,
        )
    }

    async fn raw_request(state: AppState, request: &str) -> String {
        let (client, server) = tokio::io::duplex(4096);
        let (mut cr, mut cw) = tokio::io::split(client);
        let server_task = tokio::spawn(handle_connection(server, state));
        cw.write_all(request.as_bytes()).await.unwrap();
        cw.shutdown().await.unwrap();
        let mut resp = String::new();
        tokio::io::AsyncReadExt::read_to_string(&mut cr, &mut resp)
            .await
            .unwrap();
        let _ = server_task.await;
        resp
    }

    /// For streaming endpoints (SSE never closes): read just the status line.
    async fn raw_request_status(state: AppState, request: &str) -> String {
        let (client, server) = tokio::io::duplex(4096);
        let (mut cr, mut cw) = tokio::io::split(client);
        tokio::spawn(handle_connection(server, state));
        cw.write_all(request.as_bytes()).await.unwrap();
        let mut line = String::new();
        tokio::io::AsyncBufReadExt::read_line(&mut tokio::io::BufReader::new(&mut cr), &mut line)
            .await
            .unwrap();
        line
    }

    /// Regression: an authorized POST must NOT 403. The header parser used to
    /// lowercase the header VALUE, so any mixed-case token failed the compare
    /// and every authenticated write was rejected.
    #[tokio::test]
    async fn authorized_post_prompt_returns_200() {
        let (state, mut rx) = test_state("tokEn123");
        let resp = raw_request(
            state,
            "POST /prompt HTTP/1.1\r\nhost: x\r\nauthorization: Bearer tokEn123\r\ncontent-length: 13\r\n\r\n{\"text\":\"hi\"}",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 200"),
            "got: {}",
            &resp[..resp.len().min(120)]
        );
        assert_eq!(rx.try_recv().ok().as_deref(), Some("hi"));
    }

    #[tokio::test]
    async fn wrong_token_post_returns_403() {
        let (state, _rx) = test_state("tokEn123");
        let resp = raw_request(
            state,
            "POST /prompt HTTP/1.1\r\nhost: x\r\nauthorization: Bearer wrong\r\ncontent-length: 13\r\n\r\n{\"text\":\"hi\"}",
        )
        .await;
        assert!(
            resp.starts_with("HTTP/1.1 403"),
            "got: {}",
            &resp[..resp.len().min(120)]
        );
    }

    #[tokio::test]
    async fn events_requires_valid_query_token() {
        // No token -> 403 (reads are authenticated too).
        let (state2, _rx2) = test_state("sekrit");
        let line = raw_request_status(state2, "GET /events HTTP/1.1\r\nhost: x\r\n\r\n").await;
        assert!(line.starts_with("HTTP/1.1 403"), "got: {line}");
        // Correct token in query -> 200 SSE (router strips the query string).
        let (state, _rx) = test_state("sekrit");
        let line = raw_request_status(
            state,
            "GET /events?token=sekrit HTTP/1.1\r\nhost: x\r\n\r\n",
        )
        .await;
        assert!(line.starts_with("HTTP/1.1 200"), "got: {line}");
    }
}
