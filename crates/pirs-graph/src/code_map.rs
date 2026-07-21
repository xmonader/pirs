use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::budget::{join_within_budget, DEFAULT_TOKEN_BUDGET};
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
        let graph = &*graph;
        let target = args.target.unwrap_or_default();
        let out = match args.action.as_str() {
            "symbol" => {
                let defs = graph.symbol(&target);
                if defs.is_empty() {
                    format!("no definition found for '{target}'")
                } else {
                    let lines: Vec<String> =
                        defs.iter().map(|s| fmt_symbol(s, &self.root)).collect();
                    join_within_budget(&lines, DEFAULT_TOKEN_BUDGET)
                }
            }
            "callers" => {
                let callers = graph.callers(&target);
                if callers.is_empty() {
                    format!("no callers of '{target}' in the graph")
                } else {
                    let lines: Vec<String> =
                        callers.iter().map(|s| fmt_symbol(s, &self.root)).collect();
                    join_within_budget(&lines, DEFAULT_TOKEN_BUDGET)
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
                // ranked_symbols() is the uncapped, best-first candidate
                // list; requested `limit` bounds the item count as before
                // (preserving existing `limit: 1`-style callers), and the
                // budget bisection on top of that is a safety net for when
                // even `limit` items' rendered text is unexpectedly large.
                let n = args.limit.unwrap_or(15);
                let lines: Vec<String> = graph
                    .ranked_symbols()
                    .into_iter()
                    .take(n)
                    .map(|(s, rank)| format!("{rank:.4} {}", fmt_symbol(s, &self.root)))
                    .collect();
                join_within_budget(&lines, DEFAULT_TOKEN_BUDGET)
            }
            "file_map" => {
                let path = self.root.join(&target);
                let syms = graph.file_symbols(&path);
                if syms.is_empty() {
                    format!("no symbols in {target}")
                } else {
                    let lines: Vec<String> =
                        syms.iter().map(|s| fmt_symbol(s, &self.root)).collect();
                    join_within_budget(&lines, DEFAULT_TOKEN_BUDGET)
                }
            }
            "blast" => {
                let callers = graph.callers(&target);
                let callees = graph.callees(&target);
                // One-hop transitive: callers-of-callers (unique names).
                let mut transitive: Vec<String> = Vec::new();
                let mut seen = std::collections::HashSet::new();
                for c in &callers {
                    for t in graph.callers(&c.name) {
                        let key = format!("{}:{}", t.file.display(), t.name);
                        if seen.insert(key) {
                            transitive.push(fmt_symbol(t, &self.root));
                        }
                    }
                }
                let lines: Vec<String> =
                    callers.iter().map(|s| fmt_symbol(s, &self.root)).collect();
                format!(
                    "'{target}' blast radius: {} direct caller(s), {} direct callee(s), {} second-hop caller(s)\n\
                     direct callers:\n{}\n\
                     second-hop callers:\n{}",
                    callers.len(),
                    callees.len(),
                    transitive.len(),
                    join_within_budget(&lines, DEFAULT_TOKEN_BUDGET / 2),
                    join_within_budget(&transitive, DEFAULT_TOKEN_BUDGET / 2)
                )
            }
            other => {
                format!("unknown action '{other}': use symbol|callers|callees|top|file_map|blast")
            }
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
        assert!(
            callers.contains("fn a") && callers.contains("fn c"),
            "{callers}"
        );

        let top = run(serde_json::json!({"action": "top", "limit": 1})).await;
        assert!(top.contains(" b "), "{top}");

        let blast = run(serde_json::json!({"action": "blast", "target": "b"})).await;
        assert!(blast.contains("2 direct caller(s)"), "{blast}");

        let fmap = run(serde_json::json!({"action": "file_map", "target": "a.rs"})).await;
        assert!(fmap.contains("fn a"), "{fmap}");
    }

    #[tokio::test]
    async fn callers_with_many_matches_is_capped_by_token_budget_not_returned_unbounded() {
        // "callers" had no cap of any kind before the token-budget packer —
        // a symbol called from hundreds of places would dump all of them
        // into the tool result. This proves the budget cap actually engages
        // on a real call, not just in the isolated budget:: unit tests.
        let dir = tempfile::tempdir().unwrap();
        let mut src = String::from("fn shared() {}\n");
        for i in 0..300 {
            src.push_str(&format!(
                "fn caller_number_{i:04}_with_a_reasonably_long_name() {{ shared(); }}\n"
            ));
        }
        std::fs::write(dir.path().join("a.rs"), src).unwrap();
        let graph = Arc::new(crate::LazyGraph::new(dir.path().to_path_buf()));
        let tool = CodeMapTool::new(graph, dir.path().to_path_buf());

        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"action": "callers", "target": "shared"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap()
            .content[0]
            .as_text()
            .unwrap()
            .to_string();

        assert!(out.contains("caller_number_0000"), "{out}");
        assert!(
            out.contains("more result(s) omitted to stay within the token budget"),
            "300 callers should overflow the default budget and note the drop: {out}"
        );
        assert!(
            !out.contains("caller_number_0299"),
            "the tail should have been dropped for budget, not silently kept: {out}"
        );
    }
}
