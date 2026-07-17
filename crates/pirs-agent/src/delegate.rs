use std::sync::Arc;

use async_trait::async_trait;
use pirs_ai::{CompletionOptions, LlmProvider, Message};
use serde_json::{json, Value};

use crate::agent::Agent;
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};

/// Runs a subtask in a fresh sub-agent with its own clean context and
/// returns the sub-agent's final answer. The sub-agent's tool set must not
/// include the delegate tool itself (no recursion).
pub struct DelegateTool {
    provider: Arc<dyn LlmProvider>,
    model: String,
    completion: CompletionOptions,
    make_tools: Arc<dyn Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync>,
}

impl DelegateTool {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        model: impl Into<String>,
        completion: CompletionOptions,
        make_tools: impl Fn() -> Vec<Arc<dyn AgentTool>> + Send + Sync + 'static,
    ) -> Arc<Self> {
        Arc::new(DelegateTool {
            provider,
            model: model.into(),
            completion,
            make_tools: Arc::new(make_tools),
        })
    }
}

#[async_trait]
impl AgentTool for DelegateTool {
    fn name(&self) -> &str {
        "delegate"
    }

    fn description(&self) -> &str {
        "Run a self-contained subtask in a fresh sub-agent with a clean context (same tools, no conversation history). Good for exploration, research, or isolated steps whose details would clutter the main context. Returns only the sub-agent's final answer."
    }

    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "task": {
                    "type": "string",
                    "description": "Complete, self-contained instructions for the sub-agent. It cannot see this conversation."
                }
            },
            "required": ["task"]
        })
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("delegate: run a self-contained subtask in a fresh sub-agent, get back only its answer")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let task = ctx
            .args
            .get("task")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if task.is_empty() {
            anyhow::bail!("delegate requires a non-empty task");
        }

        ctx.emit_update(format!("sub-agent started: {task}"));

        let mut agent = Agent::new(Arc::clone(&self.provider), &self.model)
            .with_tools((self.make_tools)())
            .with_completion(self.completion.clone())
            .with_compaction(None);

        let new_messages = agent.prompt(&task).await?;

        let answer = new_messages
            .iter()
            .rev()
            .find_map(|m| match m {
                Message::Assistant(a) if !a.text().trim().is_empty() => Some(a.text()),
                _ => None,
            })
            .unwrap_or_else(|| "(sub-agent produced no text answer)".to_string());

        let tool_calls: usize = new_messages
            .iter()
            .filter(|m| matches!(m, Message::ToolResult(_)))
            .count();
        let tokens: u64 = new_messages
            .iter()
            .filter_map(|m| match m {
                Message::Assistant(a) => Some(a.usage.total_tokens),
                _ => None,
            })
            .sum();

        Ok(ToolOutput::text(answer).with_details(json!({
            "subAgentToolCalls": tool_calls,
            "subAgentTokens": tokens,
        })))
    }
}
