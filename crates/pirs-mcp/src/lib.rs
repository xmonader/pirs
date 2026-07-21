use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

pub mod client;
pub mod config;
pub mod http;
pub mod tool;

pub struct McpServerHandle {
    pub name: String,
    pub client: Arc<client::Client>,
}

pub struct McpLoadResult {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub handles: Vec<McpServerHandle>,
    pub errors: Vec<String>,
}

/// Multi-server MCP health for doctor / status (degraded lifecycle report).
#[derive(Debug, Clone)]
pub struct McpDegradedReport {
    pub working: Vec<String>,
    pub failed: Vec<(String, String)>,
    pub tool_count: usize,
}

impl McpDegradedReport {
    pub fn from_load(result: &McpLoadResult) -> Self {
        let working: Vec<String> = result.handles.iter().map(|h| h.name.clone()).collect();
        let failed: Vec<(String, String)> = result
            .errors
            .iter()
            .map(|e| {
                // "MCP server 'name': reason"
                if let Some(rest) = e.strip_prefix("MCP server '") {
                    if let Some((name, reason)) = rest.split_once("': ") {
                        return (name.to_string(), reason.to_string());
                    }
                }
                ("(unknown)".into(), e.clone())
            })
            .collect();
        Self {
            working,
            failed,
            tool_count: result.tools.len(),
        }
    }

    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        if self.working.is_empty() && self.failed.is_empty() {
            out.push("mcp: no servers configured".into());
            return out;
        }
        out.push(format!(
            "mcp: {} working, {} failed, {} tools",
            self.working.len(),
            self.failed.len(),
            self.tool_count
        ));
        for n in &self.working {
            out.push(format!("  ok: {n}"));
        }
        for (n, why) in &self.failed {
            out.push(format!("  fail: {n}: {why}"));
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
        };
        assert_eq!(
            McpDegradedReport::from_load(&r).lines()[0],
            "mcp: no servers configured"
        );
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

pub async fn load_servers(cwd: &Path) -> McpLoadResult {
    let (specs, mut errors) = config::load_server_specs(cwd);
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
    }
}
