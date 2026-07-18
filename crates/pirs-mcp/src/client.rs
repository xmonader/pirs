use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

const PROTOCOL_VERSION: &str = "2025-03-26";

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct StdioClient {
    stdin: std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    stderr: Arc<Mutex<String>>,
    child: tokio::sync::Mutex<Child>,
}

#[derive(Debug, Clone)]
pub struct McpToolDef {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
}

#[derive(Debug)]
pub struct CallResult {
    pub content: Vec<pirs_ai::ContentBlock>,
    pub is_error: bool,
}

impl StdioClient {
    pub async fn spawn(
        name: &str,
        command: &str,
        args: &[String],
        env: &HashMap<String, String>,
        cwd: Option<&str>,
    ) -> anyhow::Result<Arc<Self>> {
        let mut cmd = Command::new(command);
        cmd.args(args)
            .envs(env)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        if let Some(cwd) = cwd {
            cmd.current_dir(cwd);
        }
        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn MCP server '{name}'"))?;

        let stdin = child.stdin.take().context("no stdin on MCP server")?;
        let stdout = child.stdout.take().context("no stdout on MCP server")?;
        let stderr_pipe = child.stderr.take().context("no stderr on MCP server")?;
        let stdin = std::sync::Arc::new(tokio::sync::Mutex::new(stdin));

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let stderr = Arc::new(Mutex::new(String::new()));

        {
            let pending = Arc::clone(&pending);
            let stdin_writer = std::sync::Arc::clone(&stdin);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                loop {
                    match lines.next_line().await {
                        Ok(Some(line)) => {
                            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                                continue;
                            };
                            let Some(id) = v.get("id").and_then(|i| i.as_u64()) else {
                                continue;
                            };
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
                                    let _ = tx
                                        .send(Ok(v.get("result").cloned().unwrap_or(Value::Null)));
                                }
                            }
                        }
                        Ok(None) | Err(_) => {
                            let mut all = pending.lock().unwrap();
                            for (_, tx) in all.drain() {
                                let _ = tx.send(Err("MCP server exited".to_string()));
                            }
                            break;
                        }
                    }
                }
            });
        }
        {
            let stderr = Arc::clone(&stderr);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr_pipe).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = stderr.lock().unwrap();
                    if buf.len() < 32 * 1024 {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
            });
        }

        let client = Arc::new(StdioClient {
            stdin: std::sync::Arc::clone(&stdin),
            pending,
            next_id: AtomicU64::new(1),
            stderr,
            child: tokio::sync::Mutex::new(child),
        });

        client.initialize().await?;
        Ok(client)
    }

    async fn request(
        &self,
        method: &str,
        params: Value,
        timeout: Duration,
    ) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let msg = json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        });
        let line = format!("{}\n", serde_json::to_string(&msg)?);
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => {
                let tail = self.stderr.lock().unwrap().clone();
                if tail.is_empty() {
                    bail!("MCP server dropped the request")
                } else {
                    bail!(
                        "MCP server dropped the request; stderr: {}",
                        tail.chars().take(500).collect::<String>()
                    )
                }
            }
            Err(_) => bail!("MCP request '{method}' timed out"),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        let msg = json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        });
        let line = format!("{}\n", serde_json::to_string(&msg)?);
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        Ok(())
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
                Duration::from_secs(15),
            )
            .await
            .context("MCP initialize failed")?;
        let server = result
            .get("serverInfo")
            .and_then(|s| s.get("name"))
            .and_then(|n| n.as_str())
            .unwrap_or("unknown");
        tracing::info!("MCP server initialized: {server}");
        self.notify("notifications/initialized", json!({})).await?;
        Ok(())
    }

    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        let result = self
            .request("tools/list", json!({}), Duration::from_secs(15))
            .await?;
        let tools = result
            .get("tools")
            .and_then(|t| t.as_array())
            .cloned()
            .unwrap_or_default();
        Ok(tools
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
            .collect())
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<CallResult> {
        let result = self
            .request(
                "tools/call",
                json!({ "name": name, "arguments": arguments }),
                Duration::from_secs(120),
            )
            .await?;
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
        Ok(CallResult {
            content: blocks,
            is_error,
        })
    }

    pub async fn shutdown(&self) {
        let _ = self
            .request("shutdown", json!({}), Duration::from_secs(2))
            .await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }

    pub fn stderr_tail(&self) -> String {
        self.stderr.lock().unwrap().clone()
    }
}

pub enum Client {
    Stdio(Arc<StdioClient>),
    Http(Arc<crate::http::HttpClient>),
    LegacySse(Arc<crate::http::LegacySseClient>),
}

impl Client {
    pub async fn list_tools(&self) -> anyhow::Result<Vec<McpToolDef>> {
        match self {
            Client::Stdio(c) => c.list_tools().await,
            Client::Http(c) => c.list_tools().await,
            Client::LegacySse(c) => c.list_tools().await,
        }
    }

    pub async fn call_tool(&self, name: &str, arguments: Value) -> anyhow::Result<CallResult> {
        match self {
            Client::Stdio(c) => c.call_tool(name, arguments).await,
            Client::Http(c) => c.call_tool(name, arguments).await,
            Client::LegacySse(c) => c.call_tool(name, arguments).await,
        }
    }

    pub async fn shutdown(&self) {
        match self {
            Client::Stdio(c) => c.shutdown().await,
            Client::Http(c) => c.shutdown().await,
            Client::LegacySse(c) => c.shutdown().await,
        }
    }
}
