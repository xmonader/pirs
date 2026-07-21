//! Multi-agent fleet status via `pirs-orchestrator` UDS (when the daemon is up).

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

fn orchestrator_sock() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".pirs")
        .join("orchestrator")
        .join("orchestrator.sock")
}

#[derive(Deserialize, JsonSchema)]
struct FleetArgs {
    /// Action: status | list (default status)
    #[serde(default)]
    action: Option<String>,
}

pub struct FleetTool;

#[async_trait]
impl AgentTool for FleetTool {
    fn name(&self) -> &str {
        "fleet"
    }

    fn description(&self) -> &str {
        "Query multi-agent fleet status from pirs-orchestrator (UDS). \
         Use when coordinating multiple pirs instances. Start daemon with: pirs-orchestrator serve. \
         Actions: status, list."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(FleetArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("fleet: multi-agent orchestrator status (pirs-orchestrator)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: FleetArgs = serde_json::from_value(ctx.args).unwrap_or(FleetArgs { action: None });
        let action = args.action.as_deref().unwrap_or("status");
        let sock = orchestrator_sock();
        if !sock.exists() {
            return Ok(ToolOutput::text(format!(
                "fleet: orchestrator not running (no socket at {}).\n\
                 Start with: pirs-orchestrator serve\n\
                 Spawn instances: pirs-orchestrator spawn --cwd <path> -- …\n\
                 Local multi-agent without orchestrator: use the `delegate` tool \
                 (subagents) or --strategy with fan-out phases.",
                sock.display()
            )));
        }
        // Thin IPC: send a JSONL list request (matches orchestrator client protocol).
        let line = match action {
            "list" | "status" => {
                r#"{"type":"list"}"#
            }
            other => {
                anyhow::bail!("unknown fleet action {other:?}; use status|list");
            }
        };
        let response = tokio::task::spawn_blocking({
            let sock = sock.clone();
            let line = line.to_string();
            move || uds_request(&sock, &line)
        })
        .await
        .map_err(|e| anyhow::anyhow!("fleet task: {e}"))??;
        Ok(ToolOutput::text(format!(
            "fleet ({action}) via {}:\n{response}",
            sock.display()
        )))
    }
}

fn uds_request(sock: &std::path::Path, line: &str) -> anyhow::Result<String> {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;
    let mut stream = UnixStream::connect(sock)
        .map_err(|e| anyhow::anyhow!("connect {}: {e}", sock.display()))?;
    stream.write_all(line.as_bytes())?;
    stream.write_all(b"\n")?;
    let mut reader = BufReader::new(stream);
    let mut response = String::new();
    reader.read_line(&mut response)?;
    if response.is_empty() {
        anyhow::bail!("orchestrator closed without response");
    }
    Ok(response.trim().to_string())
}

pub fn fleet_tools() -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(FleetTool)]
}
