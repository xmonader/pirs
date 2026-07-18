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
struct LsArgs {
    /// Directory to list (defaults to working directory)
    path: Option<String>,
    /// Maximum number of entries
    limit: Option<usize>,
}

pub struct LsTool {
    cwd: PathBuf,
}

impl LsTool {
    pub fn new(cwd: PathBuf) -> Self {
        LsTool { cwd }
    }
}

#[async_trait]
impl AgentTool for LsTool {
    fn name(&self) -> &str {
        "ls"
    }

    fn description(&self) -> &str {
        "List directory contents, alphabetically, including dotfiles. Directories are suffixed with /."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(LsArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("ls: list directory contents")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: LsArgs = serde_json::from_value(ctx.args)?;
        let path = paths::resolve(&self.cwd, args.path.as_deref().unwrap_or("."));
        let limit = args.limit.unwrap_or(500);

        let mut entries: Vec<String> = Vec::new();
        let read = std::fs::read_dir(&path)
            .with_context(|| format!("failed to list {}", path.display()))?;
        let mut items: Vec<(String, bool)> = Vec::new();
        for entry in read {
            let entry = entry?;
            let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
            let name = entry.file_name().to_string_lossy().to_string();
            items.push((name, is_dir));
        }
        items.sort_by(|a, b| a.0.cmp(&b.0));

        let mut size = 0;
        let total = items.len();
        for (name, is_dir) in items.into_iter().take(limit) {
            let line = if is_dir { format!("{name}/") } else { name };
            size += line.len() + 1;
            if size > MAX_BYTES {
                break;
            }
            entries.push(line);
        }
        let mut text = entries.join("\n");
        if total > limit {
            text.push_str(&format!("\n[showing {limit} of {total} entries]"));
        }
        if text.is_empty() {
            text = "(empty directory)".to_string();
        }
        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn lists_sorted_with_dir_suffix() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        std::fs::write(dir.path().join("a.txt"), "").unwrap();
        std::fs::write(dir.path().join(".hidden"), "").unwrap();
        let tool = LsTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert_eq!(out.content[0].as_text().unwrap(), ".hidden\na.txt\nsub/");
    }
}
