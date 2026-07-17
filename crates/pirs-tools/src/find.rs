use std::path::PathBuf;

use anyhow::Context as _;
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::paths;
use crate::truncate::MAX_BYTES;

#[derive(Deserialize, JsonSchema)]
struct FindArgs {
    /// Glob pattern to match file paths, e.g. "**/*.rs"
    pattern: String,
    /// Directory to search (defaults to working directory)
    path: Option<String>,
    /// Maximum number of results
    limit: Option<usize>,
}

pub struct FindTool {
    cwd: PathBuf,
}

impl FindTool {
    pub fn new(cwd: PathBuf) -> Self {
        FindTool { cwd }
    }
}

#[async_trait]
impl AgentTool for FindTool {
    fn name(&self) -> &str {
        "find"
    }

    fn description(&self) -> &str {
        "Find files by glob pattern, respecting .gitignore. Returns relative paths."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(FindArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("find: locate files by glob pattern")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: FindArgs = serde_json::from_value(ctx.args)?;
        let root = paths::resolve(&self.cwd, args.path.as_deref().unwrap_or("."));
        let limit = args.limit.unwrap_or(1000);

        let mut builder = globset::GlobSetBuilder::new();
        builder.add(globset::Glob::new(&args.pattern).with_context(|| {
            format!("invalid glob pattern: {}", args.pattern)
        })?);
        builder.add(globset::Glob::new(&format!("**/{}", args.pattern))?);
        let set = builder.build()?;

        let mut walker = ignore::WalkBuilder::new(&root);
        walker.hidden(false).require_git(false);

        let mut out = String::new();
        let mut count = 0usize;
        for entry in walker.build().flatten() {
            let path = entry.path();
            if path.is_dir() {
                continue;
            }
            let rel = path
                .strip_prefix(&self.cwd)
                .unwrap_or(path)
                .to_string_lossy()
                .to_string();
            if !set.is_match(&rel) {
                continue;
            }
            out.push_str(&rel);
            out.push('\n');
            count += 1;
            if count >= limit || out.len() > MAX_BYTES {
                break;
            }
        }
        if out.is_empty() {
            out = "No files found.".to_string();
        }
        Ok(ToolOutput::text(out.trim_end().to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    async fn run(tool: &FindTool, args: Value) -> anyhow::Result<ToolOutput> {
        tool.execute(ToolExecContext {
            tool_call_id: "t".into(),
            args,
            cancel: CancellationToken::new(),
            on_update: None,
        })
        .await
    }

    #[tokio::test]
    async fn finds_by_extension() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("src")).unwrap();
        std::fs::write(dir.path().join("src/main.rs"), "").unwrap();
        std::fs::write(dir.path().join("README.md"), "").unwrap();
        let tool = FindTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"pattern": "*.rs"})).await.unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("src/main.rs"));
        assert!(!text.contains("README.md"));
    }

    #[tokio::test]
    async fn path_pattern_matches_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("a/b")).unwrap();
        std::fs::write(dir.path().join("a/b/c.txt"), "").unwrap();
        let tool = FindTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"pattern": "b/c.txt"})).await.unwrap();
        assert!(out.content[0].as_text().unwrap().contains("a/b/c.txt"));
    }
}
