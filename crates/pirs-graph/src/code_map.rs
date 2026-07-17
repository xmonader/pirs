use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::graph::Symbol;

#[derive(Deserialize, JsonSchema)]
struct CodeMapArgs {
    /// Action: symbol | callers | callees | top | file_map | blast
    action: String,
    /// Symbol name (for symbol/callers/callees/blast) or file path (file_map)
    target: Option<String>,
    /// Max results (top, default 15)
    limit: Option<usize>,
}

pub struct CodeMapTool {
    graph: Arc<crate::LazyGraph>,
    root: PathBuf,
}

impl CodeMapTool {
    pub fn new(graph: Arc<crate::LazyGraph>, root: PathBuf) -> Self {
        CodeMapTool { graph, root }
    }
}

fn fmt_symbol(s: &Symbol, root: &std::path::Path) -> String {
    let rel = s
        .file
        .strip_prefix(root)
        .unwrap_or(&s.file)
        .to_string_lossy();
    format!("{} {} ({}:{})", s.kind.name(), s.name, rel, s.line)
}

#[async_trait]
impl AgentTool for CodeMapTool {
    fn name(&self) -> &str {
        "code_map"
    }

    fn description(&self) -> &str {
        "Query the repo's code graph: find symbol definitions, callers, callees, most-referenced symbols (top), symbols in a file, or blast radius of a symbol. Much cheaper than grep+read for understanding structure."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(CodeMapArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("code_map: query the code graph (definitions/callers/callees/top/blast) instead of blind grep")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: CodeMapArgs = serde_json::from_value(ctx.args)?;
        let graph = self.graph.get();
        let target = args.target.unwrap_or_default();
        let out = match args.action.as_str() {
            "symbol" => {
                let defs = graph.symbol(&target);
                if defs.is_empty() {
                    format!("no definition found for '{target}'")
                } else {
                    defs.iter()
                        .map(|s| fmt_symbol(s, &self.root))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "callers" => {
                let callers = graph.callers(&target);
                if callers.is_empty() {
                    format!("no callers of '{target}' in the graph")
                } else {
                    callers
                        .iter()
                        .map(|s| fmt_symbol(s, &self.root))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "callees" => {
                let callees = graph.callees(&target);
                if callees.is_empty() {
                    format!("'{target}' calls nothing in the graph")
                } else {
                    callees.join(", ")
                }
            }
            "top" => {
                let n = args.limit.unwrap_or(15);
                graph
                    .top(n)
                    .iter()
                    .map(|(s, rank)| format!("{rank:.4} {}", fmt_symbol(s, &self.root)))
                    .collect::<Vec<_>>()
                    .join("\n")
            }
            "file_map" => {
                let path = self.root.join(&target);
                let syms = graph.file_symbols(&path);
                if syms.is_empty() {
                    format!("no symbols in {target}")
                } else {
                    syms.iter()
                        .map(|s| fmt_symbol(s, &self.root))
                        .collect::<Vec<_>>()
                        .join("\n")
                }
            }
            "blast" => {
                let callers = graph.callers(&target);
                let callees = graph.callees(&target);
                format!(
                    "'{target}' blast radius: {} direct caller(s), {} direct callee(s)\ncallers:\n{}",
                    callers.len(),
                    callees.len(),
                    callers
                        .iter()
                        .map(|s| fmt_symbol(s, &self.root))
                        .collect::<Vec<_>>()
                        .join("\n")
                )
            }
            other => format!("unknown action '{other}': use symbol|callers|callees|top|file_map|blast"),
        };
        Ok(ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn code_map_queries() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.rs"),
            "fn a() { b(); }\nfn b() {}\nfn c() { b(); }\n",
        )
        .unwrap();
        let graph = Arc::new(crate::LazyGraph::new(dir.path().to_path_buf()));
        let tool = CodeMapTool::new(graph, dir.path().to_path_buf());

        let run = |args: Value| {
            let tool = &tool;
            async move {
                tool.execute(ToolExecContext {
                    tool_call_id: "t".into(),
                    args,
                    cancel: CancellationToken::new(),
                    on_update: None,
                })
                .await
                .unwrap()
                .content[0]
                .as_text()
                .unwrap()
                .to_string()
            }
        };

        let callers = run(serde_json::json!({"action": "callers", "target": "b"})).await;
        assert!(callers.contains("fn a") && callers.contains("fn c"), "{callers}");

        let top = run(serde_json::json!({"action": "top", "limit": 1})).await;
        assert!(top.contains(" b "), "{top}");

        let blast = run(serde_json::json!({"action": "blast", "target": "b"})).await;
        assert!(blast.contains("2 direct caller(s)"), "{blast}");

        let fmap = run(serde_json::json!({"action": "file_map", "target": "a.rs"})).await;
        assert!(fmap.contains("fn a"), "{fmap}");
    }
}
