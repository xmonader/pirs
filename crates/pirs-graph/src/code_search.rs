//! `code_search`: hybrid retrieval over the code graph.
//!
//! Fuses three complementary signals with reciprocal-rank fusion (RRF):
//!   - **lexical** (BM25 / tantivy) — exact terms, identifiers, error strings,
//!   - **semantic** (embeddings, optional) — meaning, for conceptual queries,
//!   - **structural** (graph centrality) — favour well-connected symbols.
//!
//! BM25 needs no model and builds in-memory instantly, so `code_search` is useful
//! the moment the graph exists. The semantic arm is best-effort: it contributes
//! when an embedding service is configured, embedding a *bounded* number of
//! symbols per call (so no single call blocks on a full cold index) and reusing
//! the persisted vectors across calls. If embeddings are absent or the service is
//! down, the tool silently runs lexical + structural only.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::EmbeddingClient;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::lexical::LexicalIndex;
use crate::store::{EmbedItem, GraphStore};

const DEFAULT_MAX_CHUNK_CHARS: usize = 2000;
const EMBED_BATCH: usize = 64;
/// RRF damping constant (standard 60): larger = flatter contribution from rank.
const RRF_K: f32 = 60.0;

#[derive(Deserialize, JsonSchema)]
struct Args {
    /// What you're looking for. Works as keywords (identifiers, error text) or a
    /// natural-language description ("where do we refresh the auth token").
    query: String,
    /// Max results (default 8).
    limit: Option<usize>,
}

pub struct CodeSearchTool {
    graph: Arc<crate::LazyGraph>,
    root: PathBuf,
    db_path: PathBuf,
    embedder: Option<EmbeddingClient>,
    max_chars: usize,
    /// Bounded number of symbols to embed per call, so a first search never
    /// blocks on a full cold index; the persisted index fills across calls.
    embed_cap: usize,
    /// Cached BM25 index, rebuilt when the symbol count changes.
    lex: Mutex<Option<(usize, LexicalIndex)>>,
}

impl CodeSearchTool {
    pub fn new(
        graph: Arc<crate::LazyGraph>,
        root: PathBuf,
        db_path: PathBuf,
        embedder: Option<EmbeddingClient>,
        max_chars: Option<usize>,
        embed_cap: Option<usize>,
    ) -> Self {
        CodeSearchTool {
            graph,
            root,
            db_path,
            embedder,
            max_chars: max_chars.unwrap_or(DEFAULT_MAX_CHUNK_CHARS),
            embed_cap: embed_cap.unwrap_or(256),
            lex: Mutex::new(None),
        }
    }

    /// BM25 hits from the cached lexical index (rebuilt if the graph grew/shrank).
    /// Synchronous — no `.await`, so the lock is never held across a suspend.
    fn lexical_hits(&self, query: &str, k: usize) -> Vec<Cand> {
        let graph = self.graph.get();
        let mut guard = self.lex.lock().unwrap();
        let stale = guard.as_ref().map(|(n, _)| *n) != Some(graph.symbols.len());
        if stale {
            match LexicalIndex::build(&graph.symbols, &self.root) {
                Ok(idx) => *guard = Some((graph.symbols.len(), idx)),
                Err(e) => {
                    tracing::warn!("lexical index build failed: {e:#}");
                    return Vec::new();
                }
            }
        }
        let Some((_, idx)) = guard.as_ref() else {
            return Vec::new();
        };
        idx.search(query, k)
            .unwrap_or_default()
            .into_iter()
            .map(|h| Cand {
                name: h.name,
                file: h.file,
                line: h.line,
            })
            .collect()
    }

    /// Semantic hits from existing + freshly (bounded) embedded vectors. Returns
    /// empty when no embedder is configured or the service is unreachable.
    async fn semantic_hits(&self, query: &str, k: usize) -> Vec<Cand> {
        let Some(embedder) = &self.embedder else {
            return Vec::new();
        };
        let qvecs = match embedder
            .embed(std::slice::from_ref(&query.to_string()))
            .await
        {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let Some(qv) = qvecs.into_iter().next() else {
            return Vec::new();
        };
        let dim = qv.len();

        // Bounded index top-up (never a full cold-embed inline).
        let pending = {
            let mut store = match GraphStore::open(&self.db_path, &self.root) {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            let _ = store.refresh();
            let _ = store.ensure_model(embedder.model(), dim);
            store.pending_embeddings(self.max_chars).unwrap_or_default()
        };
        if !pending.is_empty() {
            let take = pending.len().min(self.embed_cap);
            let (kept_idx, vecs) = self.embed_batch(embedder, &pending[..take]).await;
            let kept: Vec<EmbedItem> = kept_idx.iter().map(|&i| pending[i].clone()).collect();
            if let Ok(mut store) = GraphStore::open(&self.db_path, &self.root) {
                let _ = store.store_embeddings(&kept, &vecs);
            }
        }
        let store = match GraphStore::open(&self.db_path, &self.root) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        store
            .search(&qv, k)
            .unwrap_or_default()
            .into_iter()
            .map(|h| Cand {
                name: h.name,
                file: h.file,
                line: h.line,
            })
            .collect()
    }

    /// Embed a slice of items, batched, with a per-item truncating fallback so a
    /// dense chunk exceeding a small model's context can't abort the batch.
    async fn embed_batch(
        &self,
        embedder: &EmbeddingClient,
        items: &[EmbedItem],
    ) -> (Vec<usize>, Vec<Vec<f32>>) {
        let mut idxs = Vec::new();
        let mut vecs = Vec::new();
        for (b, chunk) in items.chunks(EMBED_BATCH).enumerate() {
            let base = b * EMBED_BATCH;
            let texts: Vec<String> = chunk.iter().map(|i| i.text.clone()).collect();
            match embedder.embed(&texts).await {
                Ok(v) => {
                    for (j, vec) in v.into_iter().enumerate() {
                        idxs.push(base + j);
                        vecs.push(vec);
                    }
                }
                Err(_) => {
                    for (j, item) in chunk.iter().enumerate() {
                        if let Some(vec) = embed_one(embedder, &item.text).await {
                            idxs.push(base + j);
                            vecs.push(vec);
                        }
                    }
                }
            }
        }
        (idxs, vecs)
    }
}

async fn embed_one(embedder: &EmbeddingClient, text: &str) -> Option<Vec<f32>> {
    let mut t = text.to_string();
    for _ in 0..6 {
        match embedder.embed(std::slice::from_ref(&t)).await {
            Ok(mut v) => return v.pop(),
            Err(_) => {
                let n = t.chars().count();
                if n <= 64 {
                    return None;
                }
                t = t.chars().take(n / 2).collect();
            }
        }
    }
    None
}

#[derive(Clone)]
struct Cand {
    name: String,
    file: PathBuf,
    line: usize,
}

fn key(c: &Cand) -> (String, usize) {
    (c.file.to_string_lossy().to_string(), c.line)
}

/// Reciprocal-rank fusion over several best-first rankings of the same candidate
/// pool. A candidate absent from a ranking simply contributes nothing from it.
fn rrf(rankings: &[Vec<(String, usize)>]) -> HashMap<(String, usize), f32> {
    let mut scores: HashMap<(String, usize), f32> = HashMap::new();
    for ranking in rankings {
        for (rank, k) in ranking.iter().enumerate() {
            *scores.entry(k.clone()).or_insert(0.0) += 1.0 / (RRF_K + rank as f32);
        }
    }
    scores
}

#[async_trait]
impl AgentTool for CodeSearchTool {
    fn name(&self) -> &str {
        "code_search"
    }

    fn description(&self) -> &str {
        "Search the codebase by keyword AND meaning: give identifiers/error text \
         or a natural-language description. Fuses BM25 lexical search, embedding \
         similarity (when configured), and code-graph centrality, returning the \
         most relevant symbols as file:line to read. Prefer this over blind grep."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(Args)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "code_search: your FIRST stop to find where code lives — one call ranks the \
             most relevant symbols as file:line for a symbol, error string, or \
             natural-language description of behavior. Use it before grep/read hunts.",
        )
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: Args = serde_json::from_value(ctx.args)?;
        let limit = args.limit.unwrap_or(8).clamp(1, 50);
        let pool = limit * 3;

        let lex = self.lexical_hits(&args.query, pool);
        let sem = self.semantic_hits(&args.query, pool).await;

        if lex.is_empty() && sem.is_empty() {
            return Ok(ToolOutput::text(
                "code_search: no matches (no supported source symbols indexed yet).",
            ));
        }

        // Build the unique candidate pool and a lookup back to each Cand.
        let mut by_key: HashMap<(String, usize), Cand> = HashMap::new();
        for c in lex.iter().chain(sem.iter()) {
            by_key.entry(key(c)).or_insert_with(|| c.clone());
        }

        // Three best-first rankings over the pool.
        let lex_rank: Vec<(String, usize)> = lex.iter().map(key).collect();
        let sem_rank: Vec<(String, usize)> = sem.iter().map(key).collect();
        let graph = self.graph.get();
        let mut struct_rank: Vec<(String, usize)> = by_key.keys().cloned().collect();
        struct_rank.sort_by_key(|k| {
            std::cmp::Reverse(
                by_key
                    .get(k)
                    .map(|c| graph.callers(&c.name).len())
                    .unwrap_or(0),
            )
        });

        let scores = rrf(&[lex_rank, sem_rank, struct_rank]);
        let mut ranked: Vec<((String, usize), f32)> = scores.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        ranked.truncate(limit);

        let arms = if self.embedder.is_some() && !sem.is_empty() {
            "lexical+semantic+graph"
        } else if self.embedder.is_some() {
            "lexical+graph (semantic index empty — see --semantic / background index)"
        } else {
            "lexical+graph"
        };
        let mut out = format!("Top {} for \"{}\" [{}]:\n", ranked.len(), args.query, arms);
        for (i, (k, _)) in ranked.iter().enumerate() {
            if let Some(c) = by_key.get(k) {
                let rel = c
                    .file
                    .strip_prefix(&self.root)
                    .unwrap_or(&c.file)
                    .to_string_lossy();
                let callers = graph.callers(&c.name).len();
                out.push_str(&format!(
                    "{}. {} ({}:{})  [{} callers]\n",
                    i + 1,
                    c.name,
                    rel,
                    c.line,
                    callers
                ));
            }
        }
        Ok(ToolOutput::text(out))
    }
}
