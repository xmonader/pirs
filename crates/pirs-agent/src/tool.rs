use serde_json::Value;

use pirs_ai::ContentBlock;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ExecutionMode {
    #[default]
    Parallel,
    Sequential,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: Vec<ContentBlock>,
    pub details: Option<Value>,
    pub terminate: bool,
}

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput {
            content: vec![ContentBlock::text(s)],
            details: None,
            terminate: false,
        }
    }

    pub fn with_details(mut self, details: Value) -> Self {
        self.details = Some(details);
        self
    }

    pub fn terminate(mut self) -> Self {
        self.terminate = true;
        self
    }
}

pub struct ToolExecContext {
    pub tool_call_id: String,
    pub args: Value,
    pub cancel: tokio_util::sync::CancellationToken,
    pub on_update: Option<std::sync::Arc<dyn Fn(String) + Send + Sync>>,
}

impl ToolExecContext {
    pub fn emit_update(&self, text: impl Into<String>) {
        if let Some(cb) = &self.on_update {
            cb(text.into());
        }
    }
}

#[async_trait::async_trait]
pub trait AgentTool: Send + Sync {
    fn name(&self) -> &str;
    fn label(&self) -> &str {
        self.name()
    }
    fn description(&self) -> &str;
    fn parameters(&self) -> Value;
    fn execution_mode(&self) -> ExecutionMode {
        ExecutionMode::Parallel
    }
    fn prompt_snippet(&self) -> Option<&str> {
        None
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput>;
}

/// Builds the schema list sent to the model. Later tools win on a name
/// collision — order preserved from each name's *last* registration — so a
/// rhai pack can override a native tool (e.g. wrapping `bash` in a sandbox)
/// by registering another tool under the same name: the model sees exactly
/// one "bash" entry, the overriding one, never two ambiguous ones. Matches
/// `prepare_call`'s dispatch lookup, which resolves the same way.
pub fn tool_defs(tools: &[std::sync::Arc<dyn AgentTool>]) -> Vec<pirs_ai::ToolDef> {
    let mut order: Vec<&str> = Vec::new();
    let mut by_name: std::collections::HashMap<&str, &std::sync::Arc<dyn AgentTool>> =
        std::collections::HashMap::new();
    for t in tools {
        let name = t.name();
        if !by_name.contains_key(name) {
            order.push(name);
        }
        by_name.insert(name, t);
    }
    order
        .into_iter()
        .map(|name| {
            let t = by_name[name];
            pirs_ai::ToolDef {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters(),
            }
        })
        .collect()
}
