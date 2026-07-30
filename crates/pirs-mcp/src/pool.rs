//! Lazy MCP connection pool with a hard concurrent live cap.
//!
//! Servers are connected only on enable / first call. Exceeding `max_live`
//! fails closed with a clear error (no unbounded process growth).

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context as _};
use tokio::sync::Mutex;

use crate::catalog::McpCatalog;
use crate::client::StdioClient;
use crate::client::{CallResult, Client, McpToolDef};
use crate::config::{ServerSpec, ServerTransport};
use crate::http::{HttpClient, LegacySseClient};

/// Default max concurrent live MCP clients (stdio processes + HTTP sessions).
pub const DEFAULT_MAX_LIVE: usize = 16;

/// Auto-eager flatten threshold: at or below this, session may connect-all.
pub const DEFAULT_EAGER_MAX: usize = 8;

pub fn max_live_from_env() -> usize {
    std::env::var("PIRS_MCP_MAX_LIVE")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_MAX_LIVE)
        .clamp(1, 256)
}

pub fn eager_max_from_env() -> usize {
    std::env::var("PIRS_MCP_EAGER_MAX")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_EAGER_MAX)
        .clamp(0, 64)
}

struct LiveServer {
    client: Arc<Client>,
    tools: Vec<McpToolDef>,
    last_used: Instant,
}

/// Concurrent-bounded pool of live MCP clients.
pub struct McpPool {
    specs: HashMap<String, ServerSpec>,
    catalog: Mutex<McpCatalog>,
    live: Mutex<HashMap<String, LiveServer>>,
    max_live: usize,
}

#[derive(Debug, Clone)]
pub struct PoolStatus {
    pub catalog_size: usize,
    pub live: Vec<String>,
    pub max_live: usize,
    pub known_tool_count: usize,
}

impl McpPool {
    pub fn new(specs: Vec<ServerSpec>, max_live: usize) -> Arc<Self> {
        let catalog = McpCatalog::from_specs(&specs);
        let map: HashMap<String, ServerSpec> =
            specs.into_iter().map(|s| (s.name.clone(), s)).collect();
        Arc::new(Self {
            specs: map,
            catalog: Mutex::new(catalog),
            live: Mutex::new(HashMap::new()),
            max_live: max_live.max(1),
        })
    }

    /// Build from catalog + specs (catalog may already have synthetic tools).
    pub fn from_catalog(specs: Vec<ServerSpec>, catalog: McpCatalog, max_live: usize) -> Arc<Self> {
        let map: HashMap<String, ServerSpec> =
            specs.into_iter().map(|s| (s.name.clone(), s)).collect();
        Arc::new(Self {
            specs: map,
            catalog: Mutex::new(catalog),
            live: Mutex::new(HashMap::new()),
            max_live: max_live.max(1),
        })
    }

    pub fn max_live(&self) -> usize {
        self.max_live
    }

    pub async fn catalog_snapshot(&self) -> McpCatalog {
        self.catalog.lock().await.clone()
    }

    pub async fn status(&self) -> PoolStatus {
        let cat = self.catalog.lock().await;
        let live = self.live.lock().await;
        let known_tool_count = cat.entries().iter().map(|e| e.known_tools.len()).sum();
        PoolStatus {
            catalog_size: cat.len(),
            live: live.keys().cloned().collect(),
            max_live: self.max_live,
            known_tool_count,
        }
    }

    pub async fn live_count(&self) -> usize {
        self.live.lock().await.len()
    }

    /// Connect (or reuse) a server and refresh its tool list into the catalog.
    /// Fails if the live pool is full and `server` is not already live.
    pub async fn enable(&self, name: &str) -> anyhow::Result<Vec<McpToolDef>> {
        {
            let mut live = self.live.lock().await;
            if let Some(ls) = live.get_mut(name) {
                ls.last_used = Instant::now();
                return Ok(ls.tools.clone());
            }
            if live.len() >= self.max_live {
                bail!(
                    "MCP live pool full ({}/{}). Disable a server with mcp_disable \
                     or raise PIRS_MCP_MAX_LIVE. Cannot enable '{name}'.",
                    live.len(),
                    self.max_live
                );
            }
        }
        let tools = self.connect_and_list(name).await?;
        Ok(tools)
    }

    /// Drop a live connection (catalog entry remains).
    pub async fn disable(&self, name: &str) -> bool {
        let mut live = self.live.lock().await;
        if let Some(ls) = live.remove(name) {
            ls.client.shutdown().await;
            true
        } else {
            false
        }
    }

    async fn connect_and_list(&self, name: &str) -> anyhow::Result<Vec<McpToolDef>> {
        let spec = self
            .specs
            .get(name)
            .with_context(|| format!("unknown MCP server '{name}' (not in catalog)"))?
            .clone();
        let client = connect_spec(&spec).await?;
        let tools = client
            .list_tools()
            .await
            .with_context(|| format!("tools/list failed for '{name}'"))?;
        {
            let mut cat = self.catalog.lock().await;
            cat.set_known_tools(name, &tools);
        }
        {
            let mut live = self.live.lock().await;
            // Re-check cap (race with parallel enables).
            if !live.contains_key(name) && live.len() >= self.max_live {
                client.shutdown().await;
                bail!(
                    "MCP live pool full ({}/{}) while enabling '{name}'",
                    live.len(),
                    self.max_live
                );
            }
            live.insert(
                name.to_string(),
                LiveServer {
                    client,
                    tools: tools.clone(),
                    last_used: Instant::now(),
                },
            );
        }
        Ok(tools)
    }

    async fn ensure_live(&self, name: &str) -> anyhow::Result<()> {
        let already = {
            let live = self.live.lock().await;
            live.contains_key(name)
        };
        if already {
            let mut live = self.live.lock().await;
            if let Some(ls) = live.get_mut(name) {
                ls.last_used = Instant::now();
            }
            return Ok(());
        }
        self.enable(name).await?;
        Ok(())
    }

    pub async fn list_tools_cached(&self, server: &str) -> anyhow::Result<Vec<McpToolDef>> {
        {
            let live = self.live.lock().await;
            if let Some(ls) = live.get(server) {
                return Ok(ls.tools.clone());
            }
        }
        // From catalog known tools without connecting when present.
        {
            let cat = self.catalog.lock().await;
            if let Some(e) = cat.get(server) {
                if !e.known_tools.is_empty() {
                    return Ok(e
                        .known_tools
                        .iter()
                        .map(|t| McpToolDef {
                            name: t.name.clone(),
                            description: t.description.clone(),
                            input_schema: t
                                .input_schema
                                .clone()
                                .unwrap_or_else(|| serde_json::json!({"type": "object"})),
                        })
                        .collect());
                }
            }
        }
        self.enable(server).await
    }

    pub async fn describe_tool(&self, server: &str, tool: &str) -> anyhow::Result<McpToolDef> {
        let tools = self.list_tools_cached(server).await?;
        // If schema-less cache miss, force live list.
        let tools = if tools
            .iter()
            .any(|t| t.name == tool && t.input_schema.is_object())
        {
            tools
        } else {
            self.ensure_live(server).await?;
            let live = self.live.lock().await;
            live.get(server).map(|ls| ls.tools.clone()).unwrap_or(tools)
        };
        tools
            .into_iter()
            .find(|t| t.name == tool)
            .with_context(|| format!("tool '{tool}' not found on server '{server}'"))
    }

    pub async fn call_tool(
        &self,
        server: &str,
        tool: &str,
        args: serde_json::Value,
    ) -> anyhow::Result<CallResult> {
        self.ensure_live(server).await?;
        let client = {
            let mut live = self.live.lock().await;
            let ls = live
                .get_mut(server)
                .with_context(|| format!("server '{server}' not live after enable"))?;
            ls.last_used = Instant::now();
            Arc::clone(&ls.client)
        };
        client.call_tool(tool, args).await
    }

    pub async fn search(&self, query: &str, limit: usize) -> Vec<crate::catalog::CatalogEntry> {
        let cat = self.catalog.lock().await;
        cat.search(query, limit).into_iter().cloned().collect()
    }
}

async fn connect_spec(spec: &ServerSpec) -> anyhow::Result<Arc<Client>> {
    match &spec.transport {
        ServerTransport::Stdio { command, args, env } => {
            let c = StdioClient::spawn(&spec.name, command, args, env, spec.cwd.as_deref()).await?;
            Ok(Arc::new(Client::Stdio(c)))
        }
        ServerTransport::Http { url, headers, mode } => {
            if mode == "sse" || (mode == "auto" && url.ends_with("/sse")) {
                let c = LegacySseClient::connect(url, headers).await?;
                Ok(Arc::new(Client::LegacySse(c)))
            } else {
                let c = HttpClient::connect(url, headers).await?;
                Ok(Arc::new(Client::Http(c)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ServerTransport;

    fn mock_script() -> String {
        format!("{}/tests/mcp_echo.py", env!("CARGO_MANIFEST_DIR"))
    }

    fn echo_spec(name: &str) -> ServerSpec {
        ServerSpec {
            name: name.into(),
            transport: ServerTransport::Stdio {
                command: "python3".into(),
                args: vec![mock_script()],
                env: HashMap::new(),
            },
            cwd: None,
        }
    }

    #[tokio::test]
    async fn enable_within_cap_lists_tools() {
        let pool = McpPool::new(vec![echo_spec("echo")], 2);
        let tools = pool.enable("echo").await.unwrap();
        assert!(tools.iter().any(|t| t.name == "echo"));
        assert_eq!(pool.live_count().await, 1);
        // Second enable reuses.
        let tools2 = pool.enable("echo").await.unwrap();
        assert_eq!(tools2.len(), tools.len());
        assert_eq!(pool.live_count().await, 1);
    }

    #[tokio::test]
    async fn pool_cap_fails_honestly() {
        // Same mock binary, distinct catalog names — each enable spawns a process.
        let specs = vec![echo_spec("a"), echo_spec("b"), echo_spec("c")];
        let pool = McpPool::new(specs, 2);
        pool.enable("a").await.unwrap();
        pool.enable("b").await.unwrap();
        assert_eq!(pool.live_count().await, 2);
        let err = pool.enable("c").await.unwrap_err().to_string();
        assert!(
            err.contains("pool full") || err.contains("full"),
            "expected pool full error, got: {err}"
        );
        assert_eq!(pool.live_count().await, 2);
        // Disable frees a slot.
        assert!(pool.disable("a").await);
        pool.enable("c").await.unwrap();
        assert_eq!(pool.live_count().await, 2);
    }

    #[tokio::test]
    async fn call_tool_lazy_connects() {
        let pool = McpPool::new(vec![echo_spec("echo")], 4);
        assert_eq!(pool.live_count().await, 0);
        let result = pool
            .call_tool("echo", "echo", serde_json::json!({"text": "pool-hi"}))
            .await
            .unwrap();
        assert!(!result.is_error);
        let text = result.content[0].as_text().unwrap();
        assert!(text.contains("pool-hi"), "{text}");
        assert_eq!(pool.live_count().await, 1);
    }

    #[tokio::test]
    async fn catalog_size_without_live() {
        let specs: Vec<_> = (0..100)
            .map(|i| {
                // Metadata-only: command that would fail if connected — we never connect.
                ServerSpec {
                    name: format!("meta-{i}"),
                    transport: ServerTransport::Stdio {
                        command: "/nonexistent-mcp-bin".into(),
                        args: vec![],
                        env: HashMap::new(),
                    },
                    cwd: None,
                }
            })
            .collect();
        let pool = McpPool::new(specs, 4);
        let st = pool.status().await;
        assert_eq!(st.catalog_size, 100);
        assert!(st.live.is_empty());
        assert_eq!(pool.live_count().await, 0);
    }
}
