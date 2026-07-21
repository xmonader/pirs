//! Agent tool to tail the first-class audit log.

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct AuditArgs {
    /// Max lines from end of audit log (default 40, max 200).
    #[serde(default)]
    last: Option<usize>,
}

pub struct AuditTailTool;

#[async_trait]
impl AgentTool for AuditTailTool {
    fn name(&self) -> &str {
        "audit_tail"
    }

    fn description(&self) -> &str {
        "Read the last N lines of the action audit log (~/.pirs/audit.jsonl). \
         Every tool call is recorded when PIRS_AUDIT is not 0."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AuditArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("audit_tail: last N actions from audit.jsonl")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: AuditArgs = serde_json::from_value(ctx.args).unwrap_or(AuditArgs { last: None });
        let n = args.last.unwrap_or(40).clamp(1, 200);
        let path = pirs_agent::default_audit_path();
        if !path.is_file() {
            return Ok(ToolOutput::text(format!(
                "no audit log yet at {} (actions will appear after tool use)",
                path.display()
            )));
        }
        let text = std::fs::read_to_string(&path)?;
        let lines: Vec<&str> = text.lines().collect();
        let start = lines.len().saturating_sub(n);
        let slice = lines[start..].join("\n");
        Ok(ToolOutput::text(format!(
            "audit {} (last {} of {} lines):\n{slice}",
            path.display(),
            lines.len() - start,
            lines.len()
        )))
    }
}
