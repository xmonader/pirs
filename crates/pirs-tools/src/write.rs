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
        let path = paths::resolve(&self.cwd, &args.path);
        let _mutation_guard = crate::filelock::lock(&path).await;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("failed to create {}", parent.display()))?;
        }
        let bytes = args.content.len();
        std::fs::write(&path, &args.content)
            .with_context(|| format!("failed to write {}", path.display()))?;
        Ok(ToolOutput::text(format!(
            "Successfully wrote {bytes} bytes to {}",
            path.display()
        )))
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
