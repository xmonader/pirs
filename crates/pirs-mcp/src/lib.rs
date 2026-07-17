use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

pub mod client;
pub mod config;
pub mod tool;

pub struct McpServerHandle {
    pub name: String,
    pub client: Arc<client::McpClient>,
}

pub struct McpLoadResult {
    pub tools: Vec<Arc<dyn AgentTool>>,
    pub handles: Vec<McpServerHandle>,
    pub errors: Vec<String>,
}

pub async fn load_servers(cwd: &Path) -> McpLoadResult {
    let (specs, mut errors) = config::load_server_specs(cwd);
    let mut tools: Vec<Arc<dyn AgentTool>> = Vec::new();
    let mut handles = Vec::new();

    for spec in specs {
        let client = match client::McpClient::spawn(&spec).await {
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
