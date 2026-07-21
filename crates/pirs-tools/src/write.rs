use std::path::PathBuf;

use anyhow::Context as _;
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::paths;

#[derive(Deserialize, JsonSchema)]
struct WriteArgs {
    /// Path to the file to write
    path: String,
    /// Content to write to the file
    content: String,
}

pub struct WriteTool {
    cwd: PathBuf,
}

impl WriteTool {
    pub fn new(cwd: PathBuf) -> Self {
        WriteTool { cwd }
    }
}

#[async_trait]
impl AgentTool for WriteTool {
    fn name(&self) -> &str {
        "write"
    }

    fn description(&self) -> &str {
        "Write content to a file, creating parent directories as needed. Overwrites existing files."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(WriteArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("write: create or overwrite a file with new content")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: WriteArgs = serde_json::from_value(ctx.args)?;
        if args.content.len() > 10 * 1024 * 1024 {
            anyhow::bail!(
                "content too large ({} bytes, cap is 10MB); write in chunks or use bash",
                args.content.len()
            );
        }
        let path = paths::resolve_contained(&self.cwd, &args.path)?;
        let _mutation_guard = crate::filelock::lock(&path).await;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = args.content.len();
        let existed = path.exists();
        let old = if existed {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        std::fs::write(&path, &args.content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        let summary = format!(
            "Successfully wrote {bytes} bytes to {} ({})",
            path.display(),
            if existed { "overwrite" } else { "create" }
        );
        let patch = if existed {
            similar::TextDiff::from_lines(&old, &args.content)
                .unified_diff()
                .context_radius(3)
                .header(&format!("a/{}", args.path), &format!("b/{}", args.path))
                .to_string()
        } else {
            let body: String = args
                .content
                .lines()
                .take(40)
                .map(|l| format!("+{l}"))
                .collect::<Vec<_>>()
                .join("\n");
            format!("+++ b/{}\n{body}", args.path)
        };
        let ui = if patch.is_empty() {
            summary.clone()
        } else {
            let p = if patch.chars().count() > 8000 {
                patch.chars().take(8000).collect::<String>() + "\n…(diff truncated)"
            } else {
                patch.clone()
            };
            format!("{summary}\n\n{p}")
        };
        Ok(ToolOutput::text_with_ui(summary, Some(ui)).with_details(serde_json::json!({
            "path": path.display().to_string(),
            "patch": patch,
            "bytes": bytes,
        })))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn writes_and_creates_parents() {
        let dir = tempfile::tempdir().unwrap();
        let tool = WriteTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"path": "a/b/c.txt", "content": "hello"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(out.content[0].as_text().unwrap().contains("5 bytes"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("a/b/c.txt")).unwrap(),
            "hello"
        );
    }
}
