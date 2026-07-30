use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

pub mod catalog;
pub mod client;
pub mod config;
pub mod http;
pub mod pool;
pub mod router;
pub mod tool;

pub use catalog::{CatalogEntry, CatalogTool, McpCatalog};
pub use pool::{
    eager_max_from_env, max_live_from_env, McpPool, PoolStatus, DEFAULT_EAGER_MAX, DEFAULT_MAX_LIVE,
};
pub use router::{router_tools, ROUTER_TOOL_NAMES};

pub struct McpServerHandle {
    pub name: String,
    pub client: Arc<client::Client>,
}

/// How MCP tools were exposed to the agent for this session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum McpLoadMode {
    /// Few servers: connect and flatten remote tools into the agent schema.
    Eager,
    /// Large catalog: only router tools (search/describe/call/status).
    CatalogRouter,
    /// Nothing configured.
    Empty,
}

pub struct McpLoadResult {
    /// Tools to register on the agent (eager remote tools and/or router).
    pub tools: Vec<Arc<dyn AgentTool>>,
    /// Live clients when eagerly connected (may be empty in catalog mode).
    pub handles: Vec<McpServerHandle>,
    pub errors: Vec<String>,
    /// Configured servers (catalog size), even when not connected.
    pub catalog_size: usize,
    pub mode: McpLoadMode,
    /// Live pool handle when using catalog/router mode (for status).
    pub pool: Option<Arc<McpPool>>,
}

/// Multi-server MCP health for doctor / status (degraded lifecycle report).
#[derive(Debug, Clone)]
pub struct McpDegradedReport {
    pub working: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub tool_count: usize,
    pub catalog_size: usize,
    pub live_count: usize,
    pub max_live: usize,
    pub mode: String,
}

impl McpDegradedReport {
    pub fn from_load(result: &McpLoadResult) -> Self {
        let working: Vec<String> = result.handles.iter().map(|h| h.name.clone()).collect();
        let failed: Vec<(String, String)> = result
            .errors
            .iter()
            .map(|e| {
                if let Some(rest) = e.strip_prefix("MCP server '") {
                    if let Some((name, reason)) = rest.split_once("': ") {
                        return (name.to_string(), reason.to_string());
                    }
                }
                ("(unknown)".into(), e.clone())
            })
            .collect();
        let (live_count, max_live) = if let Some(pool) = &result.pool {
            // Best-effort sync snapshot is not available; use handle count / env.
            (working.len(), pool.max_live())
        } else {
            (working.len(), max_live_from_env())
        };
        let mode = match result.mode {
            McpLoadMode::Eager => "eager",
            McpLoadMode::CatalogRouter => "catalog-router",
            McpLoadMode::Empty => "empty",
        };
        Self {
            working,
            failed,
            tool_count: result.tools.len(),
            catalog_size: result.catalog_size,
            live_count,
            max_live,
            mode: mode.into(),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.catalog_size == 0 && self.working.is_empty() && self.failed.is_empty() {
            out.push("mcp: no servers configured".into());
            return out;
        }
        out.push(format!(
            "mcp: mode={} catalog={} live≈{}/{} agent_tools={} working={} failed={}",
            self.mode,
            self.catalog_size,
            self.live_count,
            self.max_live,
            self.tool_count,
            self.working.len(),
            self.failed.len()
        ));
        for n in self.working.iter().take(12) {
            out.push(format!("  ok: {n}"));
        }
        if self.working.len() > 12 {
            out.push(format!("  … +{} more live/ok", self.working.len() - 12));
        }
        for (n, why) in self.failed.iter().take(8) {
            out.push(format!("  fail: {n}: {why}"));
        }
        if self.failed.len() > 8 {
            out.push(format!("  … +{} more failures", self.failed.len() - 8));
        }
        if self.mode == "catalog-router" {
            out.push(
                "  router: mcp_search mcp_describe mcp_call mcp_enable mcp_disable mcp_status"
                    .into(),
            );
        }
        out
    }

    pub fn is_fully_healthy(&self) -> bool {
        self.failed.is_empty()
    }
}

#[cfg(test)]
mod degrade_tests {
    use super::*;

    #[test]
    fn degraded_report_parses_errors() {
        let r = McpLoadResult {
            tools: vec![],
            handles: vec![],
            errors: vec!["MCP server 'foo': connection refused".into()],
            catalog_size: 1,
            mode: McpLoadMode::Eager,
            pool: None,
        };
        let rep = McpDegradedReport::from_load(&r);
        assert_eq!(rep.failed.len(), 1);
        assert_eq!(rep.failed[0].0, "foo");
        let lines = rep.lines();
        assert!(lines.iter().any(|l| l.contains("fail: foo")));
        assert!(!rep.is_fully_healthy());
    }

    #[test]
    fn empty_config_message() {
        let r = McpLoadResult {
            tools: vec![],
            handles: vec![],
            errors: vec![],
            catalog_size: 0,
            mode: McpLoadMode::Empty,
            pool: None,
        };
        assert_eq!(
            McpDegradedReport::from_load(&r).lines()[0],
            "mcp: no servers configured"
        );
    }

    #[test]
    fn catalog_router_report_mentions_router() {
        let r = McpLoadResult {
            tools: vec![],
            handles: vec![],
            errors: vec![],
            catalog_size: 1000,
            mode: McpLoadMode::CatalogRouter,
            pool: None,
        };
        let lines = McpDegradedReport::from_load(&r).lines().join("\n");
        assert!(lines.contains("catalog=1000"), "{lines}");
        assert!(lines.contains("catalog-router"), "{lines}");
        assert!(lines.contains("mcp_search"), "{lines}");
    }
}

async fn connect(spec: &config::ServerSpec) -> anyhow::Result<std::sync::Arc<client::Client>> {
    use config::ServerTransport;
    match &spec.transport {
        ServerTransport::Stdio { command, args, env } => {
            let c = client::StdioClient::spawn(&spec.name, command, args, env, spec.cwd.as_deref())
                .await?;
            Ok(std::sync::Arc::new(client::Client::Stdio(c)))
        }
        ServerTransport::Http { url, headers, mode } => {
            if mode == "sse" || (mode == "auto" && url.ends_with("/sse")) {
                let c = http::LegacySseClient::connect(url, headers).await?;
                Ok(std::sync::Arc::new(client::Client::LegacySse(c)))
            } else {
                let c = http::HttpClient::connect(url, headers).await?;
                Ok(std::sync::Arc::new(client::Client::Http(c)))
            }
        }
    }
}

/// Eager connect-all (legacy path for small configs).
async fn load_eager(specs: Vec<config::ServerSpec>, mut errors: Vec<String>) -> McpLoadResult {
    let catalog_size = specs.len();
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut handles = Vec::new();

    for spec in specs {
        let client = match connect(&spec).await {
            Ok(c) => c,
            Err(e) => {
                errors.push(format!("MCP server '{}': {e}", spec.name));
                continue;
            }
        };
        match client.list_tools().await {
            Ok(defs) => {
                tracing::info!("MCP server '{}': {} tools", spec.name, defs.len());
                for def in defs {
                    tools.push(tool::McpTool::new(&spec.name, def, Arc::clone(&client)));
                }
            }
            Err(e) => errors.push(format!(
                "MCP server '{}': tools/list failed: {e}",
                spec.name
            )),
        }
        handles.push(McpServerHandle {
            name: spec.name,
            client,
        });
    }

    McpLoadResult {
        tools,
        handles,
        errors,
        catalog_size,
        mode: if catalog_size == 0 {
            McpLoadMode::Empty
        } else {
            McpLoadMode::Eager
        },
        pool: None,
    }
}

/// Catalog + lazy pool: only router tools; no connect until mcp_enable/mcp_call.
fn load_catalog_router(specs: Vec<config::ServerSpec>, errors: Vec<String>) -> McpLoadResult {
    let catalog_size = specs.len();
    if catalog_size == 0 {
        return McpLoadResult {
            tools: vec![],
            handles: vec![],
            errors,
            catalog_size: 0,
            mode: McpLoadMode::Empty,
            pool: None,
        };
    }
    let max_live = max_live_from_env();
    let pool = McpPool::new(specs, max_live);
    let tools = router::router_tools(Arc::clone(&pool));
    McpLoadResult {
        tools,
        handles: vec![],
        errors,
        catalog_size,
        mode: McpLoadMode::CatalogRouter,
        pool: Some(pool),
    }
}

/// Load MCP for an agent session.
///
/// - **0 servers:** empty.
/// - **≤ `PIRS_MCP_EAGER_MAX` (default 8):** connect and flatten tools (compat).
/// - **Above threshold:** catalog + router only (no connect-all).
///
/// Force router for any size: `PIRS_MCP_FORCE_ROUTER=1`.
/// Force eager (dangerous at scale): `PIRS_MCP_FORCE_EAGER=1`.
pub async fn load_servers(cwd: &Path) -> McpLoadResult {
    let (specs, errors) = config::load_server_specs(cwd);
    let n = specs.len();
    if n == 0 {
        return McpLoadResult {
            tools: vec![],
            handles: vec![],
            errors,
            catalog_size: 0,
            mode: McpLoadMode::Empty,
            pool: None,
        };
    }

    let force_router = matches!(
        std::env::var("PIRS_MCP_FORCE_ROUTER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );
    let force_eager = matches!(
        std::env::var("PIRS_MCP_FORCE_EAGER").as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    );

    if force_eager && !force_router {
        return load_eager(specs, errors).await;
    }
    if force_router || n > eager_max_from_env() {
        return load_catalog_router(specs, errors);
    }
    load_eager(specs, errors).await
}

/// Build a catalog from specs without connecting (public API for tests/tools).
pub fn catalog_from_cwd(cwd: &Path) -> (McpCatalog, Vec<String>) {
    let (specs, errors) = config::load_server_specs(cwd);
    (McpCatalog::from_specs(&specs), errors)
}

#[cfg(test)]
mod scale_load_tests {
    use super::*;
    use crate::config::{ServerSpec, ServerTransport};
    use std::collections::HashMap;

    fn many_meta_specs(n: usize) -> Vec<ServerSpec> {
        (0..n)
            .map(|i| ServerSpec {
                name: format!("meta-{i}"),
                transport: ServerTransport::Stdio {
                    command: "/nonexistent".into(),
                    args: vec![],
                    env: HashMap::new(),
                },
                cwd: None,
            })
            .collect()
    }

    #[test]
    fn catalog_router_for_large_set_does_not_connect() {
        let specs = many_meta_specs(150);
        // Would fail if connect-all attempted (nonexistent binary).
        let result = load_catalog_router(specs, vec![]);
        assert_eq!(result.mode, McpLoadMode::CatalogRouter);
        assert_eq!(result.catalog_size, 150);
        assert!(result.handles.is_empty());
        assert_eq!(result.tools.len(), ROUTER_TOOL_NAMES.len());
        let names: Vec<_> = result.tools.iter().map(|t| t.name().to_string()).collect();
        for want in ROUTER_TOOL_NAMES {
            assert!(
                names.iter().any(|n| n == *want),
                "missing {want} in {names:?}"
            );
        }
    }

    #[tokio::test]
    #[allow(clippy::await_holding_lock)] // env exclusivity spans the await by design
    async fn load_servers_respects_force_router() {
        let _guard = config::TEST_ENV_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let dir = tempfile::tempdir().unwrap();
        let script = format!("{}/tests/mcp_echo.py", env!("CARGO_MANIFEST_DIR"));
        let cfg = serde_json::json!({
            "mcpServers": {
                "echo": {"command": "python3", "args": [script]}
            }
        });
        let home = dir.path().join("home");
        std::fs::create_dir_all(home.join(".pirs")).unwrap();
        std::fs::write(home.join(".pirs/mcp.json"), cfg.to_string()).unwrap();
        let cwd = dir.path().join("proj");
        std::fs::create_dir_all(&cwd).unwrap();
        let prev = std::env::var("HOME").ok();
        let prev_fr = std::env::var("PIRS_MCP_FORCE_ROUTER").ok();
        std::env::set_var("HOME", &home);
        std::env::set_var("PIRS_MCP_FORCE_ROUTER", "1");
        let result = load_servers(&cwd).await;
        if let Some(h) = prev {
            std::env::set_var("HOME", h);
        } else {
            std::env::remove_var("HOME");
        }
        match prev_fr {
            Some(v) => std::env::set_var("PIRS_MCP_FORCE_ROUTER", v),
            None => std::env::remove_var("PIRS_MCP_FORCE_ROUTER"),
        }
        assert_eq!(result.mode, McpLoadMode::CatalogRouter);
        assert_eq!(result.catalog_size, 1);
        assert_eq!(result.tools.len(), ROUTER_TOOL_NAMES.len());
        // Call via router against real mock.
        let pool = result.pool.expect("pool");
        let out = pool
            .call_tool("echo", "echo", serde_json::json!({"text": "scaled"}))
            .await
            .unwrap();
        assert!(out.content[0].as_text().unwrap().contains("scaled"));
    }
}
