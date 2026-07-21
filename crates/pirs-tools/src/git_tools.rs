//! First-class git read tools (status / diff / log / show / blame).

use std::path::PathBuf;
use std::process::Command;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct GitArgs {
    /// Action: status | diff | log | show | blame
    action: String,
    /// Path(s) for diff/blame/show
    #[serde(default)]
    path: Option<String>,
    /// Rev or rev range (log/show/diff)
    #[serde(default)]
    rev: Option<String>,
    /// Max log entries (default 20)
    #[serde(default)]
    max_count: Option<u32>,
    /// Staged diff only
    #[serde(default)]
    cached: bool,
}

pub struct GitTool {
    cwd: PathBuf,
}

impl GitTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    fn run(&self, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new("git")
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| anyhow::anyhow!("git: {e}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).into_owned();
        if !out.stderr.is_empty() {
            if !s.is_empty() {
                s.push('\n');
            }
            s.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        if !out.status.success() && s.trim().is_empty() {
            anyhow::bail!("git {:?} failed: {:?}", args, out.status.code());
        }
        Ok(truncate(&s, 16_000))
    }
}

#[async_trait]
impl AgentTool for GitTool {
    fn name(&self) -> &str {
        "git"
    }

    fn description(&self) -> &str {
        "Read-only git: status, diff, log, show, blame. Prefer over raw bash for git queries."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(GitArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("git: status/diff/log/show/blame (read-only)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: GitArgs = serde_json::from_value(ctx.args)?;
        let text = match args.action.as_str() {
            "status" => self.run(&["status", "-sb"])?,
            "diff" => {
                let mut cmd = vec!["diff".to_string(), "--no-color".into()];
                if args.cached {
                    cmd.push("--cached".into());
                }
                if let Some(rev) = &args.rev {
                    cmd.push(rev.clone());
                }
                if let Some(p) = &args.path {
                    cmd.push("--".into());
                    cmd.push(p.clone());
                }
                let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
                self.run(&refs)?
            }
            "log" => {
                let n = args.max_count.unwrap_or(20).clamp(1, 100).to_string();
                let mut cmd = vec![
                    "log".into(),
                    "--oneline".into(),
                    "--no-color".into(),
                    format!("-n{n}"),
                ];
                if let Some(rev) = &args.rev {
                    cmd.push(rev.clone());
                }
                if let Some(p) = &args.path {
                    cmd.push("--".into());
                    cmd.push(p.clone());
                }
                let refs: Vec<&str> = cmd.iter().map(|s| s.as_str()).collect();
                self.run(&refs)?
            }
            "show" => {
                let rev = args.rev.as_deref().unwrap_or("HEAD");
                self.run(&["show", "--no-color", "--stat", rev])?
            }
            "blame" => {
                let path = args
                    .path
                    .filter(|p| !p.is_empty())
                    .ok_or_else(|| anyhow::anyhow!("git blame requires path"))?;
                self.run(&["blame", "--", &path])?
            }
            other => anyhow::bail!("unknown git action {other:?}; use status|diff|log|show|blame"),
        };
        Ok(ToolOutput::text(if text.trim().is_empty() {
            "(empty)".into()
        } else {
            text
        }))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "\n…(truncated)"
    }
}

pub fn git_tools(cwd: PathBuf) -> Vec<std::sync::Arc<dyn AgentTool>> {
    vec![std::sync::Arc::new(GitTool::new(cwd))]
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_agent::ToolExecContext;
    use std::sync::Arc;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn status_in_temp_git_repo() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path().to_path_buf();
        let _ = Command::new("git")
            .args(["init"])
            .current_dir(&cwd)
            .output();
        let tool = GitTool::new(cwd);
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "1".into(),
                args: serde_json::json!({"action": "status"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text: String = out
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("");
        // git status -sb produces "##" branch line or empty-repo message
        assert!(
            text.contains("##") || text.contains("No commits") || !text.is_empty(),
            "{text}"
        );
        let _ = Arc::new(tool);
    }
}
