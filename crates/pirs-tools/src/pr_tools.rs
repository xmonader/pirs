//! PR / review workflow helpers (local git + optional `gh`).

use std::path::PathBuf;
use std::process::Command;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct PrArgs {
    /// Action: status | diff | create | checks | view
    action: String,
    /// PR title (create)
    #[serde(default)]
    title: Option<String>,
    /// PR body (create)
    #[serde(default)]
    body: Option<String>,
    /// Base branch (default: main or master)
    #[serde(default)]
    base: Option<String>,
    /// Draft PR
    #[serde(default)]
    draft: bool,
}

pub struct PrTool {
    cwd: PathBuf,
}

impl PrTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }

    fn run(&self, program: &str, args: &[&str]) -> anyhow::Result<String> {
        let out = Command::new(program)
            .args(args)
            .current_dir(&self.cwd)
            .output()
            .map_err(|e| anyhow::anyhow!("{program}: {e}"))?;
        let mut s = String::from_utf8_lossy(&out.stdout).to_string();
        if !out.stderr.is_empty() {
            s.push_str(&String::from_utf8_lossy(&out.stderr));
        }
        if !out.status.success() && s.trim().is_empty() {
            anyhow::bail!("{program} exited {:?}", out.status.code());
        }
        Ok(s)
    }
}

#[async_trait]
impl AgentTool for PrTool {
    fn name(&self) -> &str {
        "pr"
    }

    fn description(&self) -> &str {
        "Pull request workflow: git status/diff against base, create PR via gh, \
         view checks. Prefer after tests pass. Actions: status, diff, create, checks, view."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(PrArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("pr: status/diff/create/checks for pull requests (git+gh)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: PrArgs = serde_json::from_value(ctx.args)?;
        let base = args.base.as_deref().unwrap_or("main");
        let text = match args.action.as_str() {
            "status" => {
                let mut out = String::new();
                out.push_str("## git status\n");
                out.push_str(&self.run("git", &["status", "-sb"])?);
                out.push_str("\n## branch\n");
                out.push_str(&self.run("git", &["branch", "-vv"])?);
                out.push_str("\n## recent commits\n");
                out.push_str(&self.run("git", &["log", "--oneline", "-10"])?);
                out
            }
            "diff" => {
                // Prefer origin/base...HEAD, fall back to base...HEAD
                let range = format!("origin/{base}...HEAD");
                match self.run("git", &["diff", "--stat", &range]) {
                    Ok(s) if !s.trim().is_empty() => {
                        let full = self.run("git", &["diff", &range])?;
                        format!("## diff --stat {range}\n{s}\n## diff (truncated)\n{}", truncate(&full, 12000))
                    }
                    _ => {
                        let range2 = format!("{base}...HEAD");
                        let s = self.run("git", &["diff", "--stat", &range2])?;
                        let full = self.run("git", &["diff", &range2])?;
                        format!("## diff --stat {range2}\n{s}\n## diff (truncated)\n{}", truncate(&full, 12000))
                    }
                }
            }
            "create" => {
                let title = args
                    .title
                    .filter(|t| !t.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("pr create requires title"))?;
                let body = args.body.unwrap_or_default();
                let mut cmd_args = vec![
                    "pr",
                    "create",
                    "--title",
                    &title,
                    "--body",
                    &body,
                    "--base",
                    base,
                ];
                if args.draft {
                    cmd_args.push("--draft");
                }
                // Push branch first if needed.
                let _ = self.run("git", &["push", "-u", "origin", "HEAD"]);
                self.run("gh", &cmd_args)?
            }
            "checks" | "view" => {
                let sub = if args.action == "checks" {
                    "checks"
                } else {
                    "view"
                };
                match self.run("gh", &["pr", sub]) {
                    Ok(s) => s,
                    Err(e) => format!(
                        "gh pr {sub} failed: {e}\n(install GitHub CLI and auth: gh auth login)"
                    ),
                }
            }
            other => anyhow::bail!("unknown pr action {other:?}; use status|diff|create|checks|view"),
        };
        Ok(ToolOutput::text(text))
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect::<String>() + "\n…(truncated)"
    }
}

pub fn pr_tools(cwd: PathBuf) -> Vec<std::sync::Arc<dyn AgentTool>> {
    vec![std::sync::Arc::new(PrTool::new(cwd))]
}
