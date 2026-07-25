//! Model-facing MCP router: small fixed tool surface for large catalogs.
//!
//! Tools: `mcp_search`, `mcp_describe`, `mcp_call`, `mcp_enable`, `mcp_disable`, `mcp_status`.

use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::pool::McpPool;

/// Names of the fixed router tools (always this small set).
pub const ROUTER_TOOL_NAMES: &[&str] = &[
    "mcp_search",
    "mcp_describe",
    "mcp_call",
    "mcp_enable",
    "mcp_disable",
    "mcp_status",
];

pub fn router_tools(pool: Arc<McpPool>) -> Vec<Arc<dyn AgentTool>> {
    vec![
        Arc::new(McpSearchTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(McpDescribeTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(McpCallTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(McpEnableTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(McpDisableTool {
            pool: Arc::clone(&pool),
        }),
        Arc::new(McpStatusTool { pool }),
    ]
}

struct McpSearchTool {
    pool: Arc<McpPool>,
}

#[derive(Deserialize)]
struct SearchArgs {
    #[serde(default)]
    query: String,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl AgentTool for McpSearchTool {
    fn name(&self) -> &str {
        "mcp_search"
    }
    fn description(&self) -> &str {
        "Search the MCP server catalog (configured servers and known tools). \
         Does not connect every server. Use before mcp_enable / mcp_call."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "query": {"type": "string", "description": "Keyword (name, tag, tool)"},
                "limit": {"type": "integer", "description": "Max hits (default 20)"}
            }
        })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_search: find MCP servers/tools in catalog")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: SearchArgs = serde_json::from_value(ctx.args).unwrap_or(SearchArgs {
            query: String::new(),
            limit: None,
        });
        let limit = args.limit.unwrap_or(20).clamp(1, 100);
        let hits = self.pool.search(&args.query, limit).await;
        let mut lines = vec![format!(
            "mcp catalog hits={} (query={:?})",
            hits.len(),
            args.query
        )];
        for e in hits {
            let tools: Vec<_> = e.known_tools.iter().map(|t| t.name.as_str()).collect();
            let tool_s = if tools.is_empty() {
                "(tools unknown until enable)".into()
            } else {
                tools.join(", ")
            };
            lines.push(format!(
                "- {} [{}] {} | tools: {tool_s}",
                e.name, e.transport_kind, e.summary
            ));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

struct McpDescribeTool {
    pool: Arc<McpPool>,
}

#[derive(Deserialize)]
struct DescribeArgs {
    server: String,
    tool: String,
}

#[async_trait]
impl AgentTool for McpDescribeTool {
    fn name(&self) -> &str {
        "mcp_describe"
    }
    fn description(&self) -> &str {
        "Return the JSON schema and description for one MCP tool on a server. \
         May connect that server if needed."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "tool": {"type": "string"}
            },
            "required": ["server", "tool"]
        })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_describe: schema for one MCP tool")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: DescribeArgs = serde_json::from_value(ctx.args)?;
        let def = self.pool.describe_tool(&args.server, &args.tool).await?;
        Ok(ToolOutput::text(format!(
            "server={} tool={}\n{}\nschema:\n{}",
            args.server,
            def.name,
            def.description,
            serde_json::to_string_pretty(&def.input_schema).unwrap_or_default()
        )))
    }
}

struct McpCallTool {
    pool: Arc<McpPool>,
}

#[derive(Deserialize)]
struct CallArgs {
    server: String,
    tool: String,
    #[serde(default)]
    arguments: Value,
}

#[async_trait]
impl AgentTool for McpCallTool {
    fn name(&self) -> &str {
        "mcp_call"
    }
    fn description(&self) -> &str {
        "Call a tool on an MCP server (lazy-connects within the live pool cap). \
         Prefer mcp_search then mcp_describe if unsure of args."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"},
                "tool": {"type": "string"},
                "arguments": {"type": "object", "description": "Tool arguments object"}
            },
            "required": ["server", "tool"]
        })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_call: invoke MCP server tool")
    }
    fn execution_mode(&self) -> pirs_agent::ExecutionMode {
        pirs_agent::ExecutionMode::Sequential
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: CallArgs = serde_json::from_value(ctx.args)?;
        let arguments = if args.arguments.is_null() {
            json!({})
        } else {
            args.arguments
        };
        let result = self
            .pool
            .call_tool(&args.server, &args.tool, arguments)
            .await?;
        let text: String = result
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n");
        if result.is_error {
            anyhow::bail!(if text.is_empty() {
                format!("MCP {}/{} error", args.server, args.tool)
            } else {
                text
            });
        }
        Ok(ToolOutput::text(if text.is_empty() {
            "(no output)".into()
        } else {
            text
        }))
    }
}

struct McpEnableTool {
    pool: Arc<McpPool>,
}

#[derive(Deserialize)]
struct EnableArgs {
    server: String,
}

#[async_trait]
impl AgentTool for McpEnableTool {
    fn name(&self) -> &str {
        "mcp_enable"
    }
    fn description(&self) -> &str {
        "Connect one MCP server into the live pool and list its tools. \
         Fails if the live pool is full (PIRS_MCP_MAX_LIVE)."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"}
            },
            "required": ["server"]
        })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_enable: connect one MCP server (pool-capped)")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: EnableArgs = serde_json::from_value(ctx.args)?;
        let tools = self.pool.enable(&args.server).await?;
        let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
        let st = self.pool.status().await;
        Ok(ToolOutput::text(format!(
            "enabled {} live={}/{} tools: {}",
            args.server,
            st.live.len(),
            st.max_live,
            names.join(", ")
        )))
    }
}

struct McpDisableTool {
    pool: Arc<McpPool>,
}

#[derive(Deserialize)]
struct DisableArgs {
    server: String,
}

#[async_trait]
impl AgentTool for McpDisableTool {
    fn name(&self) -> &str {
        "mcp_disable"
    }
    fn description(&self) -> &str {
        "Disconnect one live MCP server (frees a pool slot). Catalog entry remains."
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "server": {"type": "string"}
            },
            "required": ["server"]
        })
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_disable: drop live MCP connection")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: DisableArgs = serde_json::from_value(ctx.args)?;
        let dropped = self.pool.disable(&args.server).await;
        let st = self.pool.status().await;
        Ok(ToolOutput::text(format!(
            "disable {} dropped={} live={}/{}",
            args.server,
            dropped,
            st.live.len(),
            st.max_live
        )))
    }
}

struct McpStatusTool {
    pool: Arc<McpPool>,
}

#[async_trait]
impl AgentTool for McpStatusTool {
    fn name(&self) -> &str {
        "mcp_status"
    }
    fn description(&self) -> &str {
        "MCP catalog size, live pool occupancy, and max concurrent connections."
    }
    fn parameters(&self) -> Value {
        json!({"type": "object", "properties": {}})
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("mcp_status: catalog vs live MCP pool")
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let st = self.pool.status().await;
        Ok(ToolOutput::text(format!(
            "mcp: catalog={} live={}/{} known_tools_cached={} live_servers={:?}",
            st.catalog_size,
            st.live.len(),
            st.max_live,
            st.known_tool_count,
            st.live
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{ServerSpec, ServerTransport};
    use pirs_agent::AgentTool;
    use std::collections::HashMap;
    use tokio_util::sync::CancellationToken;

    fn mock_script() -> String {
        format!("{}/tests/mcp_echo.py", env!("CARGO_MANIFEST_DIR"))
    }

    fn echo_spec() -> ServerSpec {
        ServerSpec {
            name: "echo".into(),
            transport: ServerTransport::Stdio {
                command: "python3".into(),
                args: vec![mock_script()],
                env: HashMap::new(),
            },
            cwd: None,
        }
    }

    async fn exec(tool: &dyn AgentTool, args: Value) -> String {
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args,
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        out.content[0].as_text().unwrap().to_string()
    }

    #[tokio::test]
    async fn router_surface_is_small_and_calls_mock() {
        let pool = McpPool::new(vec![echo_spec()], 4);
        let tools = router_tools(pool);
        assert_eq!(tools.len(), ROUTER_TOOL_NAMES.len());
        for (t, want) in tools.iter().zip(ROUTER_TOOL_NAMES) {
            assert_eq!(t.name(), *want);
        }
        let search = tools.iter().find(|t| t.name() == "mcp_search").unwrap();
        let s = exec(search.as_ref(), json!({"query": "echo"})).await;
        assert!(s.contains("echo"), "{s}");

        let call = tools.iter().find(|t| t.name() == "mcp_call").unwrap();
        let body = exec(
            call.as_ref(),
            json!({
                "server": "echo",
                "tool": "echo",
                "arguments": {"text": "router-ok"}
            }),
        )
        .await;
        assert!(body.contains("router-ok"), "{body}");

        let status = tools.iter().find(|t| t.name() == "mcp_status").unwrap();
        let st = exec(status.as_ref(), json!({})).await;
        assert!(st.contains("catalog=1"), "{st}");
        assert!(st.contains("live="), "{st}");
    }
}
