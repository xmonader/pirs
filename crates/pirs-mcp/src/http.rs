use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _};
use futures_util::StreamExt;
use serde_json::{json, Value};
use tokio::sync::oneshot;

use crate::client::{CallResult, McpToolDef};

const PROTOCOL_VERSION: &str = "2025-03-26";

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

/// Streamable-HTTP MCP transport: POST JSON-RPC, accept JSON or SSE responses,
/// propagate mcp-session-id. Legacy HTTP+SSE is handled by LegacySseClient.
pub struct HttpClient {
    url: String,
    client: reqwest::Client,
    session_id: Mutex<Option<String>>,
    next_id: AtomicU64,
}

impl HttpClient {
    pub async fn connect(
        url: &str,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut builder = reqwest::Client::builder();
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("invalid header name: {k}"))?;
            header_map.insert(
                name,
                reqwest::header::HeaderValue::from_str(v)
                    .with_context(|| format!("invalid header value for {k}"))?,
            );
        }
        builder = builder.default_headers(header_map);
        let client = HttpClient {
            url: url.trim_end_matches('/').to_string(),
            client: builder.build()?,
            session_id: Mutex::new(None),
            next_id: AtomicU64::new(1),
        };
        client.initialize().await?;
        Ok(Arc::new(client))
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": { "name": "pirs", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .context("MCP initialize failed")?;
        let server = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        tracing::info!("MCP HTTP server initialized: {server}");
        self.notify("notifications/initialized").await?;
        Ok(())
    }

    async fn post(&self, body: &Value) -> anyhow::Result<reqwest::Response> {
        let mut req = self
            .client
            .post(&self.url)
            .json(body)
            .header("accept", "application/json, text/event-stream");
        if let Some(sid) = self.session_id.lock().unwrap().clone() {
            req = req.header("mcp-session-id", sid);
        }
        Ok(req.send().await?)
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let response = tokio::time::timeout(Duration::from_secs(30), self.post(&body)).await??;
        if let Some(sid) = response
            .headers()
            .get("mcp-session-id")
            .and_then(|v| v.to_str().ok())
        {
            *self.session_id.lock().unwrap() = Some(sid.to_string());
        }
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            let truncated: String = text.chars().take(500).collect();
            bail!("MCP HTTP error {status}: {truncated}");
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        // Bound the body/SSE read too: the 30s above only covered the POST, so
        // a server that opens an SSE stream and never emits the matching id (or
        // stalls the body) would hang the tool call forever.
        let payload = tokio::time::timeout(Duration::from_secs(120), async {
            if content_type.contains("text/event-stream") {
                read_sse_for_id(response, id).await
            } else {
                Ok(response.json::<Value>().await?)
            }
        })
        .await
        .map_err(|_| anyhow::anyhow!("MCP HTTP request timed out waiting for response {id}"))??;

        if let Some(err) = payload.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("unknown error");
            bail!("{msg}");
        }
        Ok(payload.get("result").cloned().unwrap_or(Value::Null))
    }

    async fn notify(&self, method: &str) -> anyhow::Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {},
        });
        let response = tokio::time::timeout(Duration::from_secs(30), self.post(&body))
            .await
            .map_err(|_| anyhow::anyhow!("MCP notify '{method}' timed out"))??;
        if !response.status().is_success() && response.status().as_u16() != 202 {
            bail!("MCP notify '{method}' failed: HTTP {}", response.status());
        }
        Ok(())
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(parse_tool_list(&result))
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<CallResult> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        Ok(parse_call_result(&result))
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", json!({})).await;
    }
}

/// Reconnect backoff: doubles from `INITIAL` up to `MAX`, reset to `INITIAL`
/// on every successful (re)connect. A flapping legacy-SSE server must not be
/// hammered with an instant-retry loop, but also must not stay dead for the
/// rest of the session after one transient drop.
const RECONNECT_INITIAL: Duration = Duration::from_millis(500);
const RECONNECT_MAX: Duration = Duration::from_secs(30);

fn reconnect_backoff(attempt: u32) -> Duration {
    // `attempt` is 0-based; saturating so a long-lived flapping connection
    // can never overflow the shift into a bogus (or panicking) duration.
    let scale = 1u64 << attempt.min(20);
    (RECONNECT_INITIAL.saturating_mul(scale as u32)).min(RECONNECT_MAX)
}

/// Legacy HTTP+SSE transport (2024-11-05): GET <url> opens an SSE stream; the
/// server sends an `endpoint` event with the POST path; responses to POSTed
/// requests arrive on the SSE stream. The GET stream is reconnected with
/// exponential backoff if the server drops it — previously a single dropped
/// connection (network blip, server restart) killed the MCP integration for
/// the rest of the session with no way to recover.
pub struct LegacySseClient {
    /// The original SSE GET endpoint, kept so a dropped stream can be reopened.
    #[allow(dead_code)] // kept for symmetry/debuggability; reconnect closes over its own copy
    url: String,
    post_url: Arc<Mutex<String>>,
    client: reqwest::Client,
    pending: PendingMap,
    next_id: AtomicU64,
    cancel: tokio_util::sync::CancellationToken,
}

impl Drop for LegacySseClient {
    fn drop(&mut self) {
        // Otherwise the background reconnect-reader task outlives every
        // handle to this client and spins forever against a dead server.
        self.cancel.cancel();
    }
}

/// Open one SSE GET stream and read its `endpoint` event, without spawning
/// the background reader. Shared by the initial connect and every reconnect.
async fn open_sse_stream(
    client: &reqwest::Client,
    url: &str,
) -> anyhow::Result<(reqwest::Response, String)> {
    let response = client
        .get(url)
        .header("accept", "text/event-stream")
        .send()
        .await
        .context("failed to open SSE stream")?;
    if !response.status().is_success() {
        bail!("SSE connect failed: HTTP {}", response.status());
    }
    Ok((response, url.to_string()))
}

/// Drain one SSE response stream, resolving `pending` requests as their
/// matching-id message arrives, and reporting the resolved `endpoint` (the
/// POST path) back through `on_endpoint` as soon as it's seen. Returns when
/// the stream ends (server closed it, or a chunk read failed).
async fn drain_response_into_pending(
    response: reqwest::Response,
    pending: &PendingMap,
    mut on_endpoint: impl FnMut(String),
) {
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let Ok(bytes) = chunk else { break };
        buf.extend_from_slice(&bytes);
        for (event, data) in drain_sse_events(&mut buf) {
            if event == "endpoint" {
                on_endpoint(data);
            } else if event == "message" {
                if let Ok(v) = serde_json::from_str::<Value>(&data) {
                    // Only a message WITHOUT "method" is a response. A
                    // server-initiated request/notification also carries an
                    // id whose space collides with ours; never consume a
                    // pending response for it.
                    if v.get("method").is_some() {
                        // server request/notification: not ours to resolve
                    } else if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
                        let tx = pending.lock().unwrap().remove(&id);
                        if let Some(tx) = tx {
                            if let Some(err) = v.get("error") {
                                let msg = err
                                    .get("message")
                                    .and_then(|m| m.as_str())
                                    .unwrap_or("unknown error")
                                    .to_string();
                                let _ = tx.send(Err(msg));
                            } else {
                                let _ =
                                    tx.send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                            }
                        }
                    }
                }
            }
        }
    }
}

impl LegacySseClient {
    pub async fn connect(
        url: &str,
        headers: &HashMap<String, String>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
            let name = reqwest::header::HeaderName::from_bytes(k.as_bytes())
                .with_context(|| format!("invalid header name: {k}"))?;
            header_map.insert(
                name,
                reqwest::header::HeaderValue::from_str(v)
                    .with_context(|| format!("invalid header value for {k}"))?,
            );
        }
        let client = reqwest::Client::builder()
            .default_headers(header_map)
            .build()?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let (endpoint_tx, endpoint_rx) = oneshot::channel::<String>();
        let (response, url) = open_sse_stream(&client, url).await?;

        let base = base_url(&url);
        let compute_post_url = move |endpoint: String| {
            if endpoint.starts_with("http") {
                endpoint
            } else {
                format!("{base}{}", endpoint.trim_start_matches('/'))
            }
        };

        let cancel = tokio_util::sync::CancellationToken::new();
        let post_url = Arc::new(Mutex::new(String::new()));

        {
            let pending = Arc::clone(&pending);
            let post_url = Arc::clone(&post_url);
            let reader_client = client.clone();
            let reader_url = url.clone();
            let cancel = cancel.clone();
            let mut first_response = Some(response);
            let mut endpoint_tx = Some(endpoint_tx);
            let compute_post_url = compute_post_url.clone();
            tokio::spawn(async move {
                let mut attempt: u32 = 0;
                loop {
                    if cancel.is_cancelled() {
                        return;
                    }
                    let response = match first_response.take() {
                        Some(r) => Ok(r),
                        None => open_sse_stream(&reader_client, &reader_url)
                            .await
                            .map(|(r, _)| r),
                    };
                    let response = match response {
                        Ok(r) => {
                            attempt = 0; // connected — reset backoff
                            r
                        }
                        Err(e) => {
                            tracing::warn!("MCP SSE reconnect attempt {attempt} failed: {e:#}");
                            tokio::select! {
                                _ = tokio::time::sleep(reconnect_backoff(attempt)) => {}
                                _ = cancel.cancelled() => return,
                            }
                            attempt = attempt.saturating_add(1);
                            continue;
                        }
                    };
                    let pending = Arc::clone(&pending);
                    let post_url = Arc::clone(&post_url);
                    let compute_post_url = compute_post_url.clone();
                    let on_endpoint = |endpoint: String| {
                        let resolved = compute_post_url(endpoint);
                        *post_url.lock().unwrap() = resolved.clone();
                        if let Some(tx) = endpoint_tx.take() {
                            let _ = tx.send(resolved);
                        }
                    };
                    tokio::select! {
                        _ = drain_response_into_pending(response, &pending, on_endpoint) => {}
                        _ = cancel.cancelled() => return,
                    }
                    if cancel.is_cancelled() {
                        return;
                    }
                    tracing::warn!(
                        "MCP SSE stream closed unexpectedly, reconnecting in {:?}",
                        reconnect_backoff(attempt)
                    );
                    // In-flight requests at the moment of the drop are lost —
                    // a fresh stream can't answer a request tied to the old
                    // one — but this reconnect loop means *future* requests
                    // recover instead of every subsequent call failing for
                    // the rest of the session.
                    {
                        let mut all = pending.lock().unwrap();
                        for (_, tx) in all.drain() {
                            let _ = tx.send(Err("SSE stream closed".to_string()));
                        }
                    }
                    tokio::select! {
                        _ = tokio::time::sleep(reconnect_backoff(attempt)) => {}
                        _ = cancel.cancelled() => return,
                    }
                    attempt = attempt.saturating_add(1);
                }
            });
        }

        // The reader task already wrote the resolved URL into `post_url`
        // before sending on this channel — awaiting it is purely to bound
        // how long we wait for the initial handshake.
        tokio::time::timeout(Duration::from_secs(15), endpoint_rx)
            .await
            .context("timed out waiting for SSE endpoint event")??;

        let client = Arc::new(LegacySseClient {
            url,
            post_url,
            client,
            pending,
            next_id: AtomicU64::new(1),
            cancel,
        });
        client.initialize().await?;
        Ok(client)
    }

    async fn initialize(&self) -> anyhow::Result<()> {
        let result = self
            .request(
                "initialize",
                json!({
                    "protocolVersion": "2024-11-05",
                    "capabilities": {},
                    "clientInfo": { "name": "pirs", "version": env!("CARGO_PKG_VERSION") },
                }),
            )
            .await
            .context("MCP initialize failed")?;
        let server = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        tracing::info!("MCP SSE server initialized: {server}");
        self.notify("notifications/initialized").await?;
        Ok(())
    }

    async fn send(&self, body: &Value) -> anyhow::Result<()> {
        let post_url = self.post_url.lock().unwrap().clone();
        let response = self.client.post(&post_url).json(body).send().await?;
        if !response.status().is_success() && response.status().as_u16() != 202 {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            // char-based truncation: byte-slicing a multibyte error page panics
            let truncated: String = text.chars().take(300).collect();
            bail!("MCP SSE POST error {status}: {truncated}");
        }
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let body = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        self.send(&body).await?;
        match tokio::time::timeout(Duration::from_secs(60), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("MCP server dropped the request"),
            Err(_) => bail!("MCP request '{method}' timed out"),
        }
    }

    async fn notify(&self, method: &str) -> anyhow::Result<()> {
        let body = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": {},
        });
        self.send(&body).await
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        let result = self.request("tools/list", json!({})).await?;
        Ok(parse_tool_list(&result))
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<CallResult> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
            )
            .await?;
        Ok(parse_call_result(&result))
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", json!({})).await;
    }
}

pub fn parse_tool_list(result: &Value) -> Vec<McpToolDef> {
    result
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default()
        .into_iter()
        .map(|t| McpToolDef {
            name: t
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("unnamed")
                .to_string(),
            description: t
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string(),
            input_schema: t
                .get("inputSchema")
                .cloned()
                .unwrap_or(json!({"type": "object"})),
        })
        .collect()
}

pub fn parse_call_result(result: &Value) -> CallResult {
    let is_error = result
        .get("isError")
        .and_then(|e| e.as_bool())
        .unwrap_or(false);
    let content = result
        .get("content")
        .and_then(|c| c.as_array())
        .cloned()
        .unwrap_or_default();
    let blocks: Vec<pirs_ai::ContentBlock> = content
        .into_iter()
        .filter_map(|c| match c.get("type").and_then(|t| t.as_str()) {
            Some("text") => c
                .get("text")
                .and_then(|t| t.as_str())
                .map(pirs_ai::ContentBlock::text),
            Some("image") => Some(pirs_ai::ContentBlock::Image {
                data: c
                    .get("data")
                    .and_then(|d| d.as_str())
                    .unwrap_or("")
                    .to_string(),
                mime_type: c
                    .get("mimeType")
                    .and_then(|m| m.as_str())
                    .unwrap_or("image/png")
                    .to_string(),
            }),
            _ => None,
        })
        .collect();
    CallResult {
        content: blocks,
        is_error,
    }
}

async fn read_sse_for_id(response: reqwest::Response, want_id: u64) -> anyhow::Result<Value> {
    let mut stream = response.bytes_stream();
    let mut buf = Vec::new();
    while let Some(chunk) = stream.next().await {
        let bytes = chunk?;
        buf.extend_from_slice(&bytes);
        for (_event, data) in drain_sse_events(&mut buf) {
            let Ok(v) = serde_json::from_str::<Value>(&data) else {
                continue;
            };
            if v.get("id").and_then(|i| i.as_u64()) == Some(want_id) {
                return Ok(v);
            }
        }
    }
    bail!("SSE stream ended without a response for request {want_id}")
}

pub fn drain_sse_events(buf: &mut Vec<u8>) -> Vec<(String, String)> {
    let mut events = Vec::new();
    loop {
        let boundary = find_boundary(buf);
        let Some((pos, len)) = boundary else { break };
        let raw: Vec<u8> = buf.drain(..pos + len).collect();
        let text = String::from_utf8_lossy(&raw[..pos]).to_string();
        let mut event = "message".to_string();
        let mut data = String::new();
        for line in text.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event = rest.trim().to_string();
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if !data.is_empty() {
            events.push((event, data));
        }
    }
    events
}

fn find_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = find_sub(buf, b"\r\n\r\n").map(|p| (p, 4));
    let lf = find_sub(buf, b"\n\n").map(|p| (p, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_sub(h: &[u8], n: &[u8]) -> Option<usize> {
    h.windows(n.len()).position(|w| w == n)
}

fn base_url(url: &str) -> String {
    match url.find("://") {
        Some(scheme_end) => {
            let after = &url[scheme_end + 3..];
            match after.find('/') {
                Some(path_start) => url[..scheme_end + 3 + path_start + 1].to_string(),
                None => format!("{url}/"),
            }
        }
        None => url.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reconnect_backoff_doubles_then_caps() {
        // Pinned exact values, not just "increasing" — a real production
        // incident elsewhere came from a backoff formula that looked right
        // but was actually `n^attempt` instead of `initial * 2^attempt`.
        assert_eq!(reconnect_backoff(0), Duration::from_millis(500));
        assert_eq!(reconnect_backoff(1), Duration::from_millis(1000));
        assert_eq!(reconnect_backoff(2), Duration::from_millis(2000));
        assert_eq!(reconnect_backoff(3), Duration::from_millis(4000));
        assert_eq!(
            reconnect_backoff(6),
            Duration::from_secs(32).min(RECONNECT_MAX)
        );
        assert_eq!(
            reconnect_backoff(6),
            RECONNECT_MAX,
            "must cap at 30s, not keep doubling"
        );
        assert_eq!(
            reconnect_backoff(50),
            RECONNECT_MAX,
            "must never overflow for a long-flapping connection"
        );
    }

    #[test]
    fn drain_events_parses() {
        let mut buf =
            b"event: endpoint\ndata: /messages?sid=1\n\nevent: message\ndata: {\"id\":1}\n\n"
                .to_vec();
        let events = drain_sse_events(&mut buf);
        assert_eq!(events.len(), 2);
        assert_eq!(
            events[0],
            ("endpoint".to_string(), "/messages?sid=1".to_string())
        );
        assert_eq!(events[1].0, "message");
    }

    #[test]
    fn base_url_extraction() {
        assert_eq!(base_url("http://host:3000/sse"), "http://host:3000/");
        assert_eq!(base_url("https://x.io"), "https://x.io/");
    }
}
