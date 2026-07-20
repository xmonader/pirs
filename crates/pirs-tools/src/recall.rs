use async_trait::async_trait;
use pirs_agent::memory::{MemoryHit, MemoryStore};
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::EmbeddingClient;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;

/// Search session memory: every tool result ever spilled, plus everything
/// compaction demoted out of the context window. Lexical search (the
/// default) is scoped to the current session; `mode: "semantic"` — only
/// available when an embedder is configured — searches across *every*
/// session instead, since recalling something from a previous run is the
/// actual point of embeddings-based recall (see `pirs_agent::memory`'s
/// module docs for why `search_semantic` is never session-scoped).
#[derive(Default)]
pub struct RecallTool {
    store: Option<Arc<MemoryStore>>,
    embedder: Option<EmbeddingClient>,
}

#[derive(Deserialize, JsonSchema)]
struct RecallArgs {
    /// Search terms. In lexical mode (default) each word is matched as an
    /// FTS phrase; in semantic mode the whole query is embedded and matched
    /// by meaning.
    query: String,
    /// Max results (default 8)
    limit: Option<u32>,
    /// "lexical" (default, current session only) or "semantic" (meaning-based,
    /// across all sessions — only available when an embedder is configured)
    mode: Option<String>,
}

impl RecallTool {
    /// Explicit store (tests, embeddings without a global).
    pub fn with_store(store: Arc<MemoryStore>) -> Self {
        Self {
            store: Some(store),
            embedder: None,
        }
    }

    /// Adds semantic-mode support on top of the global memory store.
    pub fn with_embedder(embedder: EmbeddingClient) -> Self {
        Self {
            store: None,
            embedder: Some(embedder),
        }
    }

    #[cfg(test)]
    fn with_store_and_embedder(store: Arc<MemoryStore>, embedder: EmbeddingClient) -> Self {
        Self {
            store: Some(store),
            embedder: Some(embedder),
        }
    }
}

fn format_hits(hits: Vec<MemoryHit>) -> String {
    let mut out = String::new();
    for h in hits {
        out.push_str(&format!(
            "[{} {}] {}\n",
            h.kind,
            h.name,
            h.snippet.replace('\n', " ")
        ));
    }
    out
}

#[async_trait]
impl AgentTool for RecallTool {
    fn name(&self) -> &str {
        "recall"
    }
    fn description(&self) -> &str {
        if self.embedder.is_some() {
            "Search your own history by keyword (mode: lexical, default, current session) or by meaning (mode: semantic, across every past session). Use when you need a command output, error, or detail from earlier — including a previous session — that is no longer in context."
        } else {
            "Search your own session history (past tool results and compacted-away messages) by keyword. Use when you need a command output, error, or detail from earlier that is no longer in context."
        }
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
        let limit = args.limit.unwrap_or(8) as usize;
        let semantic = args.mode.as_deref() == Some("semantic");

        let hits = if semantic {
            let Some(embedder) = &self.embedder else {
                return Ok(ToolOutput::text(
                    "semantic recall is not available: no embedder is configured for this session \
                     (use mode: \"lexical\", or run with --semantic/--embed-model to enable it)",
                ));
            };
            store.search_semantic(embedder, &args.query, limit).await
        } else {
            store.search(&args.query, limit)
        };

        if hits.is_empty() {
            return Ok(ToolOutput::text(format!(
                "no memory hits for {:?}",
                args.query
            )));
        }
        Ok(ToolOutput::text(format_hits(hits)))
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

    /// A deterministic fake `/v1/embeddings` server: any input containing
    /// "database" gets one fixed vector, everything else another — enough
    /// to prove semantic recall actually calls through to
    /// `search_semantic`/an embedder, not that it reproduces real model
    /// behavior.
    fn spawn_fake_embedder() -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..4 {
                let Ok((mut sock, _)) = listener.accept() else {
                    break;
                };
                let mut buf = vec![0u8; 65536];
                let n = sock.read(&mut buf).unwrap_or(0);
                let req = String::from_utf8_lossy(&buf[..n]);
                let body = req.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
                let v: serde_json::Value =
                    serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
                let inputs = v
                    .get("input")
                    .and_then(|i| i.as_array())
                    .cloned()
                    .unwrap_or_default();
                let data: Vec<serde_json::Value> = inputs
                    .iter()
                    .enumerate()
                    .map(|(i, s)| {
                        let is_db = s.as_str().unwrap_or("").to_lowercase().contains("database");
                        let vec = if is_db { [1.0, 0.0] } else { [0.0, 1.0] };
                        serde_json::json!({"index": i, "embedding": vec})
                    })
                    .collect();
                let payload = serde_json::json!({ "data": data }).to_string();
                let resp = format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
                let _ = sock.write_all(resp.as_bytes());
            }
        });
        format!("http://{addr}/v1")
    }

    #[tokio::test]
    async fn semantic_mode_finds_a_memory_from_a_different_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.set_session("sess-A");
        store.add("tool_result", "bash", "the database connection timed out");
        store.set_session("sess-B");

        let base = spawn_fake_embedder();
        let embedder = pirs_ai::EmbeddingClient::new(base, "mock", None);
        store.embed_pending(&embedder, 10).await;

        let tool = RecallTool::with_store_and_embedder(store, embedder);
        let lexical_miss = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"query": "database"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(
            lexical_miss.content[0]
                .as_text()
                .unwrap()
                .contains("no memory hits"),
            "default (lexical) mode stays scoped to sess-B and must miss the sess-A memory"
        );

        let semantic_hit = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"query": "database outage", "mode": "semantic"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = semantic_hit.content[0].as_text().unwrap();
        assert!(
            text.contains("database"),
            "semantic mode should reach across sessions to find it: {text}"
        );
    }

    #[tokio::test]
    async fn semantic_mode_without_an_embedder_is_a_clear_error_not_a_silent_miss() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add("tool_result", "bash", "the database connection timed out");
        let tool = RecallTool::with_store(store);
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"query": "database", "mode": "semantic"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("not available"), "{text}");
    }
}
