use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use serde_json::Value;

use crate::client::{Client, McpToolDef};

pub struct McpTool {
    server_name: String,
    def: McpToolDef,
    client: Arc<Client>,
    full_name: String,
}

impl McpTool {
    pub fn new(server_name: &str, def: McpToolDef, client: Arc<Client>) -> Arc<Self> {
        Arc::new(McpTool {
            full_name: format!("mcp_{}_{}", sanitize(server_name), sanitize(&def.name)),
            server_name: server_name.to_string(),
            def,
            client,
        })
    }
}

fn sanitize(name: &str) -> String {
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

#[async_trait]
impl AgentTool for McpTool {
    fn name(&self) -> &str {
        &self.full_name
    }

    fn label(&self) -> &str {
        &self.def.name
    }

    fn description(&self) -> &str {
        &self.def.description
    }

    fn parameters(&self) -> Value {
        self.def.input_schema.clone()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        None
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let result = self
            .client
            .call_tool(&self.def.name, ctx.args.clone())
            .await
            .map_err(|e| {
                anyhow::anyhow!(
                    "MCP tool {}/{} failed: {e}",
                    self.server_name,
                    self.def.name
                )
            })?;
        let out = ToolOutput {
            content: if result.content.is_empty() {
                vec![pirs_ai::ContentBlock::text("(no output)")]
            } else {
                result.content
            },
            details: None,
            terminate: false,
        };
        if result.is_error {
            let text: String = out
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .collect::<Vec<_>>()
                .join("\n");
            anyhow::bail!(if text.is_empty() {
                format!("MCP tool {} returned an error", self.def.name)
            } else {
                text
            });
        }
        Ok(out)
    }
}
