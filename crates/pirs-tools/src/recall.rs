use async_trait::async_trait;
use pirs_agent::memory::MemoryStore;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

/// Search session memory: every tool result ever spilled, plus everything
/// compaction demoted out of the context window.
#[derive(Default)]
pub struct RecallTool {
    store: Option<Arc<MemoryStore>>,
}

#[derive(Deserialize, JsonSchema)]
struct RecallArgs {
    /// Search terms (FTS-ranked; each word is matched as a phrase)
    query: String,
    /// Max results (default 8)
    limit: Option<u32>,
}

impl RecallTool {
    /// Explicit store (tests, embeddings without a global).
    pub fn with_store(store: Arc<MemoryStore>) -> Self {
        Self { store: Some(store) }
    }
}

#[async_trait]
impl AgentTool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        "Search your own session history (past tool results and compacted-away messages) by keyword. Use when you need a command output, error, or detail from earlier that is no longer in context."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RecallArgs)).unwrap()
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: RecallArgs = serde_json::from_value(ctx.args)?;
        let store = match &self.store {
            Some(s) => Some(Arc::clone(s)),
            None => pirs_agent::memory::global(),
        };
        let Some(store) = store else {
            return Ok(ToolOutput::text("session memory is not enabled"));
        };
        let hits = store.search(&args.query, args.limit.unwrap_or(8) as usize);
        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "no memory hits for {:?}",
                args.query
            )));
        }
        let mut out = String::new();
        for h in hits {
            out.push_str(&format!(
                "[{} {}] {}\n",
                h.kind,
                h.name,
                h.snippet.replace('\n', " ")
            ));
        }
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn recall_finds_spilled_result() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add(
            "tool_result",
            "bash",
            "build failed: linker error undefined reference to foo",
        );
        let tool = RecallTool::with_store(store);
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"query": "linker error"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("tool_result bash"), "{text}");
        assert!(text.contains(">>>linker<<<"), "{text}");

        let miss = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"query": "nonexistent-zebra"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(miss.content[0]
            .as_text()
            .unwrap()
            .contains("no memory hits"));
    }
}
