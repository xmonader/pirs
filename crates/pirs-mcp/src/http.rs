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
            bail!("MCP HTTP error {status}: {}", &text[..text.len().min(500)]);
        }
        let content_type = response
            .headers()
            .get("content-type")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("")
            .to_string();

        let payload = if content_type.contains("text/event-stream") {
            read_sse_for_id(response, id).await?
        } else {
            let v: Value = response.json().await?;
            v
        };

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
        let response = self.post(&body).await?;
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

/// Legacy HTTP+SSE transport (2024-11-05): GET <url> opens an SSE stream; the
/// server sends an `endpoint` event with the POST path; responses to POSTed
/// requests arrive on the SSE stream.
pub struct LegacySseClient {
    post_url: String,
    client: reqwest::Client,
    pending: PendingMap,
    next_id: AtomicU64,
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
        let response = client
            .get(url)
            .header("accept", "text/event-stream")
            .send()
            .await
            .context("failed to open SSE stream")?;
        if !response.status().is_success() {
            bail!("SSE connect failed: HTTP {}", response.status());
        }

        {
            let pending = Arc::clone(&pending);
            let mut endpoint_tx = Some(endpoint_tx);
            tokio::spawn(async move {
                let mut stream = response.bytes_stream();
                let mut buf = Vec::new();
                while let Some(chunk) = stream.next().await {
                    let Ok(bytes) = chunk else { break };
                    buf.extend_from_slice(&bytes);
                    for (event, data) in drain_sse_events(&mut buf) {
                        if event == "endpoint" {
                            if let Some(tx) = endpoint_tx.take() {
                                let _ = tx.send(data);
                            }
                        } else if event == "message" {
                            if let Ok(v) = serde_json::from_str::<Value>(&data) {
                                if let Some(id) = v.get("id").and_then(|i| i.as_u64()) {
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
                                            let _ = tx.send(Ok(v
                                                .get("result")
                                                .cloned()
                                                .unwrap_or(Value::Null)));
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
                let mut all = pending.lock().unwrap();
                for (_, tx) in all.drain() {
                    let _ = tx.send(Err("SSE stream closed".to_string()));
                }
            });
        }

        let endpoint = tokio::time::timeout(Duration::from_secs(15), endpoint_rx)
            .await
            .context("timed out waiting for SSE endpoint event")??;
        let base = base_url(url);
        let post_url = if endpoint.starts_with("http") {
            endpoint
        } else {
            format!("{base}{}", endpoint.trim_start_matches('/'))
        };

        let client = Arc::new(LegacySseClient {
            post_url,
            client,
            pending,
            next_id: AtomicU64::new(1),
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
        let response = self.client.post(&self.post_url).json(body).send().await?;
        if !response.status().is_success() && response.status().as_u16() != 202 {
            let status = response.status().as_u16();
            let text = response.text().await.unwrap_or_default();
            bail!(
                "MCP SSE POST error {status}: {}",
                &text[..text.len().min(300)]
            );
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
