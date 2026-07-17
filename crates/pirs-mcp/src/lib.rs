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

async fn connect(spec: &config::ServerSpec) -> anyhow::Result<std::sync::Arc<client::Client>> {
    use config::ServerTransport;
    match &spec.transport {
        ServerTransport::Stdio {
            command,
            args,
            env,
        } => {
            let c = client::StdioClient::spawn(
                &spec.name,
                command,
                args,
                env,
                spec.cwd.as_deref(),
            )
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
            Err(e) => errors.push(format!("MCP server '{}': tools/list failed: {e}", spec.name)),
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
