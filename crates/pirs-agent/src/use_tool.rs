use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use serde_json::{json, Value};

use crate::agent_loop::VisibleTools;
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};

/// Meta-tool that lets the model load hidden tools into the active set
/// (pi's addedToolNames pattern). Built when tool diet is enabled.
pub struct UseTool {
    visible: VisibleTools,
    catalog: Arc<Mutex<Vec<(String, String, Value)>>>,
}

impl UseTool {
    pub fn new(visible: &VisibleTools, tools: &[Arc<dyn AgentTool>]) -> Arc<Self> {
        let catalog = tools
            .iter()
            .map(|t| {
                (
                    t.name().to_string(),
                    t.description().to_string(),
                    t.parameters(),
                )
            })
            .collect();
        Arc::new(UseTool {
            visible: Arc::clone(visible),
            catalog: Arc::new(Mutex::new(catalog)),
        })
    }

    pub fn default_visible() -> std::collections::HashSet<String> {
        // edit_block is visible by default: weak models match SEARCH/REPLACE
        // more reliably than nested oldText/newText JSON (aider lesson).
        ["bash", "read", "edit", "edit_block", "write", "use_tool"]
            .iter()
            .map(|s| s.to_string())
            .collect()
    }
}

#[async_trait]
impl AgentTool for UseTool {
    fn name(&self) -> &str {
        "use_tool"
    }

    fn description(&self) -> &str {
        "Load an additional tool into this session. Use when you need a tool that is not currently available (e.g. grep, find, ls, or extension tools). Returns the tool's schema."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": { "type": "string", "description": "Name of the tool to load" }
            },
            "required": ["name"]
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("use_tool: load a hidden tool by name (grep, find, ls, ...)")
    }

    fn execution_mode(&self) -> crate::tool::ExecutionMode {
        crate::tool::ExecutionMode::Sequential
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let name = ctx
            .args
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        let catalog = self.catalog.lock().unwrap();
        let found = catalog
            .iter()
            .find(|(n, _, _)| *n == name)
            .map(|(_, d, s)| (d.clone(), s.clone()));
        let Some((description, schema)) = found else {
            let available = catalog
                .iter()
                .map(|(n, _, _)| n.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            anyhow::bail!("Unknown tool '{name}'. Available tools: {available}");
        };
        drop(catalog);

        let already = self.visible.lock().unwrap().contains(&name);
        self.visible.lock().unwrap().insert(name.clone());
        let note = if already { " (was already loaded)" } else { "" };
        Ok(ToolOutput::text(format!(
            "Tool '{name}' loaded{note}.\nDescription: {description}\nParameters schema: {}",
            serde_json::to_string(&schema).unwrap_or_default()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Dummy;

    #[async_trait]
    impl AgentTool for Dummy {
        fn name(&self) -> &str {
            "grep"
        }
        fn description(&self) -> &str {
            "search"
        }
        fn parameters(&self) -> Value {
            json!({"type":"object","properties":{"pattern":{"type":"string"}}})
        }
        async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
            Ok(ToolOutput::text(""))
        }
    }

    #[tokio::test]
    async fn use_tool_loads_and_reports_schema() {
        let visible: VisibleTools = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(Dummy)];
        let use_tool = UseTool::new(&visible, &tools);
        let out = use_tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: json!({"name": "grep"}),
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(out.content[0]
            .as_text()
            .unwrap()
            .contains("Tool 'grep' loaded"));
        assert!(visible.lock().unwrap().contains("grep"));
    }

    #[tokio::test]
    async fn use_tool_unknown_lists_available() {
        let visible: VisibleTools = Arc::new(Mutex::new(std::collections::HashSet::new()));
        let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(Dummy)];
        let use_tool = UseTool::new(&visible, &tools);
        let err = use_tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: json!({"name": "nope"}),
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("Available tools: grep"));
    }
}
