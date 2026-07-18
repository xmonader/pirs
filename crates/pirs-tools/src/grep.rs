use std::path::PathBuf;

use anyhow::Context as _;
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::paths;
use crate::truncate::{truncate_line, GREP_LINE_MAX, MAX_BYTES};

#[derive(Deserialize, JsonSchema)]
struct GrepArgs {
    /// Regex pattern to search for (or literal string if literal=true)
    pattern: String,
    /// File or directory to search (defaults to working directory)
    path: Option<String>,
    /// Glob filter for file names, e.g. "*.rs"
    glob: Option<String>,
    /// Case-insensitive matching
    #[serde(rename = "ignoreCase")]
    ignore_case: Option<bool>,
    /// Treat pattern as a literal string, not a regex
    literal: Option<bool>,
    /// Number of context lines around each match
    context: Option<usize>,
    /// Maximum number of matches
    limit: Option<usize>,
}

pub struct GrepTool {
    cwd: PathBuf,
}

impl GrepTool {
    pub fn new(cwd: PathBuf) -> Self {
        GrepTool { cwd }
    }
}

#[async_trait]
impl AgentTool for GrepTool {
    fn name(&self) -> &str {
        "grep"
    }

    fn description(&self) -> &str {
        "Search file contents with a regex pattern, respecting .gitignore. Output is path:line: text with optional context lines."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(GrepArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("grep: search file contents by regex (path/glob/context/limit options)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: GrepArgs = serde_json::from_value(ctx.args)?;
        let pattern = if args.literal.unwrap_or(false) {
            regex::escape(&args.pattern)
        } else {
            args.pattern.clone()
        };
        let re = regex::RegexBuilder::new(&pattern)
            .case_insensitive(args.ignore_case.unwrap_or(false))
            .build()
            .with_context(|| format!("invalid pattern: {}", args.pattern))?;

        let root = paths::resolve(&self.cwd, args.path.as_deref().unwrap_or("."));
        let limit = args.limit.unwrap_or(100);
        let context_lines = args.context.unwrap_or(0);

        let mut builder = ignore::WalkBuilder::new(&root);
        builder
            .hidden(false)
            .require_git(false)
            .filter_entry(|e| e.file_name() != ".git");
        let glob_filter = match &args.glob {
            Some(g) => {
                let mut overrides = ignore::overrides::OverrideBuilder::new(&root);
                overrides.add(g)?;
                Some(overrides.build()?)
            }
            None => None,
        };

        let mut matches = 0usize;
        let mut out = String::new();
        let mut limit_hit = false;

        'walk: for entry in builder.build().flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            if let Some(f) = &glob_filter {
                if !f.matched(path, false).is_whitelist() {
                    continue;
                }
            }
            let Ok(content) = std::fs::read_to_string(path) else {
                continue;
            };
            let lines: Vec<&str> = content.lines().collect();
            let rel = path
                .strip_prefix(&self.cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();

            let mut last_printed: i64 = -1i64;
            let mut i = 0;
            while i < lines.len() {
                if re.is_match(lines[i]) {
                    matches += 1;
                    if matches > limit {
                        limit_hit = true;
                        break 'walk;
                    }
                    let ctx_start = i
                        .saturating_sub(context_lines)
                        .max((last_printed + 1) as usize);
                    let ctx_end = (i + context_lines + 1).min(lines.len());
                    for (n, line) in lines.iter().enumerate().take(ctx_end).skip(ctx_start) {
                        let display = truncate_line(line, GREP_LINE_MAX);
                        if n == i {
                            out.push_str(&format!("{rel}:{}: {display}\n", n + 1));
                        } else {
                            out.push_str(&format!("{rel}-{}- {display}\n", n + 1));
                        }
                        last_printed = n as i64;
                    }
                    if out.len() > MAX_BYTES {
                        break 'walk;
                    }
                    i += 1;
                } else {
                    i += 1;
                }
            }
        }

        if out.is_empty() && !limit_hit {
            out = "No matches found.".to_string();
        }
        if limit_hit {
            out.push_str(&format!(
                "\n[{limit} matches limit reached. Use limit={} or a more specific pattern.]",
                limit * 2
            ));
        }
        Ok(ToolOutput::text(out.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    async fn run(tool: &GrepTool, args: Value) -> anyhow::Result<ToolOutput> {
        tool.execute(ToolExecContext {
            tool_call_id: "t".into(),
            args,
            cancel: CancellationToken::new(),
            on_update: None,
        })
        .await
    }

    #[tokio::test]
    async fn finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "hello\nworld\nhello again\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"pattern": "hello"}))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("a.txt:1: hello"));
        assert!(text.contains("a.txt:3: hello again"));
    }

    #[tokio::test]
    async fn respects_limit() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "x\nx\nx\nx\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"pattern": "x", "limit": 2}))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("2 matches limit reached"));
    }

    #[tokio::test]
    async fn context_lines_dash_prefixed() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.txt"), "before\nmatch\nafter\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"pattern": "match", "context": 1}))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("a.txt-1- before"));
        assert!(text.contains("a.txt:2: match"));
        assert!(text.contains("a.txt-3- after"));
    }

    #[tokio::test]
    async fn glob_filters_files() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "needle\n").unwrap();
        std::fs::write(dir.path().join("a.txt"), "needle\n").unwrap();
        let tool = GrepTool::new(dir.path().to_path_buf());
        let out = run(
            &tool,
            serde_json::json!({"pattern": "needle", "glob": "*.rs"}),
        )
        .await
        .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("a.rs"));
        assert!(!text.contains("a.txt"));
    }
}
