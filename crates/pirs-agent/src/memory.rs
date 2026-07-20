//! Session memory as retrieval, not summarization. Tool results spill into a
//! SQLite FTS store as they happen; compaction demotes dropped messages into
//! the same store instead of destroying them. The `recall` tool searches it,
//! giving effectively unbounded sessions on small context windows.
//!
//! On top of that lexical (FTS5 BM25) search, this module also supports
//! embeddings-based recall: `embed_pending`/`search_semantic` populate and
//! query a companion `mem_vec` table (cosine similarity, brute-force —
//! memory stores are small enough that an ANN index isn't warranted), and
//! `search_semantic` re-ranks its candidate pool with MMR (`mmr_select`) so
//! the top results aren't just near-duplicates of each other. Unlike the
//! lexical `search`, which scopes to the current session by default,
//! semantic search always looks across every session — recalling something
//! from a *previous* run is the actual point of embeddings here, not
//! keyword precision within the current one. `consolidate` is the other
//! half of "cross-session memory": without it, a long-running project
//! accumulates near-identical rows turn after turn (the same recurring
//! error, the same repeated instruction) with nothing ever collapsing them;
//! it merges near-duplicates (by cosine similarity) across all sessions,
//! always keeping the more recent of a pair.

use std::path::Path;
use std::sync::{Arc, Mutex};

use pirs_ai::Message;

pub struct MemoryStore {
    conn: Mutex<rusqlite::Connection>,
    /// Current session id; stamped on every row and used to scope `search`.
    /// Empty = unscoped (tests / embedded use).
    session: Mutex<String>,
}

fn vec_to_blob(v: &[f32]) -> Vec<u8> {
    v.iter().flat_map(|f| f.to_le_bytes()).collect()
}

fn blob_to_vec(b: &[u8]) -> Vec<f32> {
    b.chunks_exact(4)
        .map(|c| f32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Maximal Marginal Relevance: greedily selects `k` candidates that balance
/// relevance to `query_vec` against redundancy with what's already been
/// selected, so a top-k doesn't collapse into near-duplicates of the single
/// best match. `lambda` in `[0, 1]`: `1.0` is pure relevance (no diversity
/// penalty at all), `0.0` ignores relevance entirely after the first pick.
/// `candidates` should already be a reasonably-sized pool (callers
/// pre-filter by cosine similarity) — this is O(k * n) against it.
pub fn mmr_select<T: Clone>(
    query_vec: &[f32],
    candidates: Vec<(T, Vec<f32>)>,
    k: usize,
    lambda: f32,
) -> Vec<T> {
    let mut remaining = candidates;
    let mut selected: Vec<(T, Vec<f32>)> = Vec::new();
    while !remaining.is_empty() && selected.len() < k {
        let mut best_idx = 0usize;
        let mut best_score = f32::NEG_INFINITY;
        for (i, (_, v)) in remaining.iter().enumerate() {
            let relevance = pirs_ai::cosine(query_vec, v);
            // Nothing to be redundant with yet on the first pick, so it's
            // always the most relevant candidate regardless of lambda —
            // otherwise lambda=0.0 would make even the *first* pick an
            // arbitrary tie (score = 0 for everyone) instead of "ignore
            // relevance once there's something to diversify against".
            let score = if selected.is_empty() {
                relevance
            } else {
                let redundancy = selected
                    .iter()
                    .map(|(_, sv)| pirs_ai::cosine(v, sv))
                    .fold(f32::MIN, f32::max);
                lambda * relevance - (1.0 - lambda) * redundancy
            };
            if score > best_score {
                best_score = score;
                best_idx = i;
            }
        }
        selected.push(remaining.remove(best_idx));
    }
    selected.into_iter().map(|(id, _)| id).collect()
}

#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub kind: String,
    pub name: String,
    pub snippet: String,
    pub ts: i64,
}

impl MemoryStore {
    pub fn open(path: &Path) -> Result<Arc<Self>, rusqlite::Error> {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let conn = rusqlite::Connection::open(path)?;
        // Migrate: earlier builds created `mem` without a `session` column.
        // It's a cache, so on the old schema we just rebuild it rather than
        // silently failing every scoped insert/query.
        let has_session = conn.prepare("SELECT session FROM mem LIMIT 1").is_ok();
        if !has_session {
            let _ = conn.execute_batch("DROP TABLE IF EXISTS mem;");
        }
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS mem USING fts5(
                kind UNINDEXED, name, text, ts UNINDEXED, session UNINDEXED
            );
            CREATE TABLE IF NOT EXISTS mem_vec (mem_rowid INTEGER PRIMARY KEY, vector BLOB NOT NULL);
            CREATE TABLE IF NOT EXISTS mem_meta (key TEXT PRIMARY KEY, value TEXT NOT NULL);",
        )?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
            session: Mutex::new(String::new()),
        }))
    }

    /// Scope subsequent adds and searches to this session id.
    pub fn set_session(&self, id: &str) {
        *self.session.lock().unwrap() = id.to_string();
    }

    pub fn add(&self, kind: &str, name: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let session = self.session.lock().unwrap().clone();
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO mem (kind, name, text, ts, session) VALUES (?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![kind, name, text, ts, session],
        );
    }

    /// Demote a range of messages (about to be compacted away) into the store.
    pub fn add_messages(&self, messages: &[Message]) {
        for m in messages {
            match m {
                Message::User(u) => {
                    let text = match &u.content {
                        pirs_ai::UserContent::Text(t) => t.clone(),
                        pirs_ai::UserContent::Blocks(b) => b
                            .iter()
                            .filter_map(|b| b.as_text())
                            .collect::<Vec<_>>()
                            .join("\n"),
                    };
                    self.add("user", "user", &text);
                }
                Message::Assistant(a) => {
                    let text = a.text();
                    if !text.trim().is_empty() {
                        self.add("assistant", "assistant", &text);
                    }
                }
                // Tool results are already spilled at execution time; skipping
                // them here avoids a duplicate row for every demoted result.
                Message::ToolResult(_) => {}
            }
        }
    }

    /// FTS search scoped to the current session (all sessions if none is set).
    /// Query is sanitized: each whitespace token becomes a quoted phrase, so
    /// user input containing FTS operators can't error or inject.
    pub fn search(&self, query: &str, limit: usize) -> Vec<MemoryHit> {
        self.search_scoped(query, limit, false)
    }

    /// Search across every session, regardless of the current one.
    pub fn search_all(&self, query: &str, limit: usize) -> Vec<MemoryHit> {
        self.search_scoped(query, limit, true)
    }

    fn search_scoped(&self, query: &str, limit: usize, all: bool) -> Vec<MemoryHit> {
        let terms: Vec<String> = query
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let match_q = terms.join(" ");
        let session = self.session.lock().unwrap().clone();
        // Scope to the current session unless asked for all, or unless no
        // session is set (embedded/test use stays unscoped).
        let scoped = !all && !session.is_empty();
        let conn = self.conn.lock().unwrap();
        let sql = if scoped {
            "SELECT kind, name, snippet(mem, 2, '>>>', '<<<', '...', 24), ts
             FROM mem WHERE mem MATCH ?1 AND session = ?3 ORDER BY rank LIMIT ?2"
        } else {
            "SELECT kind, name, snippet(mem, 2, '>>>', '<<<', '...', 24), ts
             FROM mem WHERE mem MATCH ?1 ORDER BY rank LIMIT ?2"
        };
        let mut stmt = match conn.prepare(sql) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let map = |r: &rusqlite::Row| {
            Ok(MemoryHit {
                kind: r.get(0)?,
                name: r.get(1)?,
                snippet: r.get(2)?,
                ts: r.get(3)?,
            })
        };
        let rows = if scoped {
            stmt.query_map(rusqlite::params![match_q, limit as i64, session], map)
        } else {
            stmt.query_map(rusqlite::params![match_q, limit as i64], map)
        };
        match rows {
            Ok(rows) => rows.flatten().collect(),
            Err(_) => Vec::new(),
        }
    }

    /// Records which embedding model/dimension `mem_vec` currently holds
    /// vectors for, wiping stale vectors on a change — embeddings from
    /// different models aren't comparable, so switching models must not
    /// silently mix them into the same cosine search. Returns `true` if a
    /// change was detected (and the table cleared), `false` if unchanged.
    pub fn ensure_model(&self, model: &str, dim: usize) -> bool {
        let conn = self.conn.lock().unwrap();
        let cur_model: Option<String> = conn
            .query_row(
                "SELECT value FROM mem_meta WHERE key = 'embed_model'",
                [],
                |r| r.get(0),
            )
            .ok();
        let cur_dim: Option<String> = conn
            .query_row(
                "SELECT value FROM mem_meta WHERE key = 'embed_dim'",
                [],
                |r| r.get(0),
            )
            .ok();
        if cur_model.as_deref() == Some(model)
            && cur_dim.as_deref() == Some(dim.to_string().as_str())
        {
            return false;
        }
        let _ = conn.execute("DELETE FROM mem_vec", []);
        let _ = conn.execute(
            "INSERT INTO mem_meta (key, value) VALUES ('embed_model', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [model],
        );
        let _ = conn.execute(
            "INSERT INTO mem_meta (key, value) VALUES ('embed_dim', ?1)
             ON CONFLICT(key) DO UPDATE SET value = ?1",
            [dim.to_string()],
        );
        true
    }

    /// Memory rows with no vector yet, up to `limit` — the batch a caller
    /// should hand to an embedder next.
    pub fn pending_embeddings(&self, limit: usize) -> Vec<(i64, String)> {
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT mem.rowid, mem.text FROM mem
             LEFT JOIN mem_vec ON mem.rowid = mem_vec.mem_rowid
             WHERE mem_vec.mem_rowid IS NULL
             LIMIT ?1",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        stmt.query_map([limit as i64], |r| Ok((r.get(0)?, r.get(1)?)))
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
    }

    fn store_embedding(&self, rowid: i64, vector: &[f32]) {
        let blob = vec_to_blob(vector);
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO mem_vec (mem_rowid, vector) VALUES (?1, ?2)
             ON CONFLICT(mem_rowid) DO UPDATE SET vector = ?2",
            rusqlite::params![rowid, blob],
        );
    }

    /// Embeds up to `batch` pending rows and stores the resulting vectors.
    /// Returns how many were embedded. Callers are expected to call
    /// `ensure_model` first so a model switch doesn't mix incompatible
    /// vectors — this method doesn't call it itself, since it doesn't know
    /// the dimension until the embed call returns.
    pub async fn embed_pending(&self, embedder: &pirs_ai::EmbeddingClient, batch: usize) -> usize {
        let pending = self.pending_embeddings(batch);
        if pending.is_empty() {
            return 0;
        }
        let texts: Vec<String> = pending.iter().map(|(_, t)| t.clone()).collect();
        let vecs = match embedder.embed(&texts).await {
            Ok(v) => v,
            Err(_) => return 0,
        };
        let mut n = 0;
        for ((rowid, _), vec) in pending.iter().zip(vecs.iter()) {
            self.store_embedding(*rowid, vec);
            n += 1;
        }
        n
    }

    /// Semantic recall: embeds `query`, cosine-scores it against every
    /// vectorized memory (brute force — memory stores are small enough that
    /// this is cheap, same tradeoff pirs-graph's own embedding search
    /// makes), takes a generous top pool, and re-ranks that pool with MMR so
    /// the final results aren't just near-duplicates of the single best
    /// match. Unlike `search`, this is never session-scoped: the point of
    /// embeddings-based recall is finding relevant context from *other*
    /// sessions, which lexical `search` already can't reach across without
    /// `search_all`.
    pub async fn search_semantic(
        &self,
        embedder: &pirs_ai::EmbeddingClient,
        query: &str,
        limit: usize,
    ) -> Vec<MemoryHit> {
        if query.trim().is_empty() || limit == 0 {
            return Vec::new();
        }
        let qvec = match embedder.embed(&[query.to_string()]).await {
            Ok(mut v) if !v.is_empty() => v.remove(0),
            _ => return Vec::new(),
        };

        let all: Vec<(i64, Vec<f32>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = match conn.prepare("SELECT mem_rowid, vector FROM mem_vec") {
                Ok(s) => s,
                Err(_) => return Vec::new(),
            };
            stmt.query_map([], |r| {
                let rowid: i64 = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                Ok((rowid, blob_to_vec(&blob)))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
        };
        if all.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(i64, Vec<f32>, f32)> = all
            .into_iter()
            .map(|(id, v)| {
                let s = pirs_ai::cosine(&qvec, &v);
                (id, v, s)
            })
            .collect();
        scored.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        let pool = (limit * 4).max(20).min(scored.len());
        scored.truncate(pool);
        let candidates: Vec<(i64, Vec<f32>)> =
            scored.into_iter().map(|(id, v, _)| (id, v)).collect();
        let selected_ids = mmr_select(&qvec, candidates, limit, 0.7);

        let conn = self.conn.lock().unwrap();
        selected_ids
            .into_iter()
            .filter_map(|id| {
                conn.query_row(
                    "SELECT kind, name, text, ts FROM mem WHERE rowid = ?1",
                    [id],
                    |r| {
                        let text: String = r.get(2)?;
                        Ok(MemoryHit {
                            kind: r.get(0)?,
                            name: r.get(1)?,
                            snippet: text.chars().take(240).collect(),
                            ts: r.get(3)?,
                        })
                    },
                )
                .ok()
            })
            .collect()
    }

    /// Merges near-duplicate memories (cosine similarity >= `threshold`)
    /// across every session, always keeping the more recent of a pair —
    /// the actual "consolidation" in cross-session memory: without this, a
    /// long-running project accumulates near-identical rows turn after
    /// turn (the same recurring error, the same repeated instruction) with
    /// nothing ever collapsing them. Only considers rows that already have
    /// a vector (`embed_pending` populates those); returns the number of
    /// rows removed. `dry_run` computes that count without deleting.
    pub fn consolidate(&self, threshold: f32, dry_run: bool) -> usize {
        let all: Vec<(i64, Vec<f32>)> = {
            let conn = self.conn.lock().unwrap();
            let mut stmt = match conn.prepare(
                "SELECT mem_vec.mem_rowid, mem_vec.vector FROM mem_vec
                 JOIN mem ON mem.rowid = mem_vec.mem_rowid
                 ORDER BY mem.ts ASC",
            ) {
                Ok(s) => s,
                Err(_) => return 0,
            };
            stmt.query_map([], |r| {
                let rowid: i64 = r.get(0)?;
                let blob: Vec<u8> = r.get(1)?;
                Ok((rowid, blob_to_vec(&blob)))
            })
            .map(|rows| rows.flatten().collect())
            .unwrap_or_default()
        };

        // Oldest-first: when row i duplicates an already-kept row, i is
        // strictly newer (we walk in ts ASC order), so it replaces the
        // older one in `kept` and the older one is what gets deleted —
        // consolidation always keeps the most recent of a cluster.
        let mut kept: Vec<usize> = Vec::new();
        let mut to_delete: Vec<i64> = Vec::new();
        for i in 0..all.len() {
            let dup_of = kept
                .iter()
                .position(|&j| pirs_ai::cosine(&all[i].1, &all[j].1) >= threshold);
            match dup_of {
                Some(pos) => {
                    to_delete.push(all[kept[pos]].0);
                    kept[pos] = i;
                }
                None => kept.push(i),
            }
        }

        let n = to_delete.len();
        if !dry_run && n > 0 {
            let conn = self.conn.lock().unwrap();
            for id in &to_delete {
                let _ = conn.execute("DELETE FROM mem WHERE rowid = ?1", [id]);
                let _ = conn.execute("DELETE FROM mem_vec WHERE mem_rowid = ?1", [id]);
            }
        }
        n
    }
}

static GLOBAL: std::sync::RwLock<Option<Arc<MemoryStore>>> = std::sync::RwLock::new(None);

/// Open the process-wide store (idempotent-ish: last call wins, but callers
/// set it once at startup).
pub fn init_global(path: &Path) -> Result<Arc<MemoryStore>, rusqlite::Error> {
    let store = MemoryStore::open(path)?;
    *GLOBAL.write().unwrap() = Some(Arc::clone(&store));
    Ok(store)
}

pub fn global() -> Option<Arc<MemoryStore>> {
    GLOBAL.read().unwrap().clone()
}

/// Scope the process-wide store to a session id (call after `init_global`).
pub fn set_session(id: &str) {
    if let Some(store) = global() {
        store.set_session(id);
    }
}

/// Test-only escape hatch: the global is process-wide, so tests that init it
/// must reset it to avoid polluting each other.
#[cfg(test)]
pub fn clear_global_for_tests() {
    *GLOBAL.write().unwrap() = None;
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::{AssistantMessage, ContentBlock};

    #[test]
    fn spill_and_recall_ranked() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add(
            "tool_result",
            "bash",
            "the deployment failed with exit code 137",
        );
        store.add("tool_result", "read", "fn main() { println!(\"hi\"); }");
        store.add("user", "user", "why did the deploy OOM?");

        let hits = store.search("deployment failed", 10);
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].name, "bash");
        assert!(
            hits[0].snippet.contains(">>>deployment<<<"),
            "{:?}",
            hits[0]
        );

        // FTS-operator-heavy input is sanitized, not an error.
        let _ = store.search("\" OR * NEAR(", 10);
        let _ = store.search("OOM", 5);
        assert_eq!(store.search("", 5).len(), 0);
    }

    #[test]
    fn demote_messages_into_store() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add_messages(&[
            Message::user("please refactor the parser"),
            Message::Assistant(AssistantMessage {
                content: vec![ContentBlock::text("done: rewrote parse_expr")],
                ..Default::default()
            }),
        ]);
        assert_eq!(store.search("refactor parser", 5).len(), 1);
        assert_eq!(store.search("parse_expr", 5).len(), 1);
    }

    #[test]
    fn recall_is_scoped_to_current_session() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();

        store.set_session("sess-A");
        store.add("tool_result", "bash", "alpha failure in module A");
        store.set_session("sess-B");
        store.add("tool_result", "bash", "beta failure in module B");

        // In session B, the unrelated session-A row is invisible by default.
        assert_eq!(store.search("failure", 10).len(), 1);
        assert_eq!(store.search("alpha", 10).len(), 0);
        // Cross-session search still sees everything.
        assert_eq!(store.search_all("failure", 10).len(), 2);
        assert_eq!(store.search_all("alpha", 10).len(), 1);

        // Switching back re-scopes.
        store.set_session("sess-A");
        assert_eq!(store.search("alpha", 10).len(), 1);
        assert_eq!(store.search("beta", 10).len(), 0);
    }

    #[test]
    fn demote_skips_tool_results_to_avoid_double_spill() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        // Tool results spill at execution; demoting them again would duplicate.
        store.add_messages(&[Message::ToolResult(pirs_ai::ToolResultMessage {
            tool_call_id: "1".into(),
            tool_name: "bash".into(),
            content: vec![ContentBlock::text("unique_tool_output_xyz")],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        })]);
        assert_eq!(store.search("unique_tool_output_xyz", 5).len(), 0);
    }

    // ── Embeddings / MMR / consolidation ─────────────────────────────────

    /// A deterministic fake `/v1/embeddings` server: maps each input string
    /// to one of three orthogonal 3-dim vectors by keyword, so cosine
    /// similarity behaves predictably in tests (same category -> 1.0,
    /// different category -> 0.0) without needing a real embedding model.
    fn spawn_fake_embedder(conns: usize) -> String {
        use std::io::{Read, Write};
        use std::net::TcpListener;

        fn fake_vector(text: &str) -> [f32; 3] {
            let t = text.to_lowercase();
            if t.contains("database") {
                [1.0, 0.0, 0.0]
            } else if t.contains("rust") {
                [0.0, 1.0, 0.0]
            } else {
                [0.0, 0.0, 1.0]
            }
        }

        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::thread::spawn(move || {
            for _ in 0..conns {
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
                        let vec = fake_vector(s.as_str().unwrap_or(""));
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

    #[test]
    fn mmr_select_prefers_diverse_results_over_pure_relevance_ranking() {
        let query = vec![1.0, 0.0, 0.0];
        // Two near-identical "database" docs (both highly relevant to the
        // query) and one "rust" doc, less relevant but not redundant with
        // either database doc.
        let candidates = vec![
            ("db-1", vec![1.0, 0.0, 0.0]),
            ("db-2", vec![0.95, 0.05, 0.0]),
            ("rust-1", vec![0.4, 0.9, 0.0]),
        ];
        // Pure relevance (lambda=1.0) would pick both database docs first.
        let pure_relevance = mmr_select(&query, candidates.clone(), 2, 1.0);
        assert_eq!(pure_relevance, vec!["db-1", "db-2"]);

        // Pure diversity (lambda=0.0): the first pick is still by relevance
        // (nothing to diversify against yet), but the second pick ignores
        // relevance entirely and must be whichever remaining candidate is
        // LEAST similar to what's already selected -- unambiguously rust-1,
        // regardless of exact cosine values, since it's the only one that
        // isn't a near-duplicate of db-1.
        let diversified = mmr_select(&query, candidates, 2, 0.0);
        assert_eq!(diversified, vec!["db-1", "rust-1"]);
    }

    #[test]
    fn mmr_select_returns_fewer_than_k_when_candidates_run_out() {
        let query = vec![1.0, 0.0, 0.0];
        let candidates = vec![("only", vec![1.0, 0.0, 0.0])];
        let selected = mmr_select(&query, candidates, 5, 0.5);
        assert_eq!(selected, vec!["only"]);
    }

    #[test]
    fn ensure_model_reports_change_and_clears_vectors() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        assert!(
            store.ensure_model("model-a", 3),
            "first call always reports a change"
        );
        store.store_embedding(1, &[1.0, 0.0, 0.0]);

        assert!(
            !store.ensure_model("model-a", 3),
            "same model/dim again should report no change"
        );
        assert!(
            store.ensure_model("model-b", 3),
            "a different model must report a change"
        );
        // The vector stored under model-a must be gone now that the model
        // changed -- otherwise a later cosine search would silently mix
        // vectors from two incompatible embedding spaces.
        let conn = store.conn.lock().unwrap();
        let count: i64 = conn
            .query_row("SELECT COUNT(*) FROM mem_vec", [], |r| r.get(0))
            .unwrap();
        assert_eq!(count, 0, "model switch must clear existing vectors");
    }

    #[tokio::test]
    async fn embed_pending_embeds_rows_and_stops_marking_them_pending() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add("tool_result", "bash", "the database connection failed");
        store.add(
            "tool_result",
            "cargo",
            "rust compile error: mismatched types",
        );
        assert_eq!(store.pending_embeddings(10).len(), 2);

        let base = spawn_fake_embedder(4);
        let embedder = pirs_ai::EmbeddingClient::new(base, "mock", None);
        let n = store.embed_pending(&embedder, 10).await;
        assert_eq!(n, 2);
        assert_eq!(
            store.pending_embeddings(10).len(),
            0,
            "both rows should now have vectors"
        );
    }

    #[tokio::test]
    async fn search_semantic_finds_relevant_memories_across_sessions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();

        store.set_session("sess-A");
        store.add("tool_result", "bash", "the database connection timed out");
        store.set_session("sess-B");
        store.add("tool_result", "cargo", "totally unrelated rust build note");

        let base = spawn_fake_embedder(4);
        let embedder = pirs_ai::EmbeddingClient::new(base, "mock", None);
        store.embed_pending(&embedder, 10).await;

        // Still scoped to session B, but semantic search must reach across
        // sessions -- unlike lexical `search`, which would miss session A
        // entirely from here.
        assert_eq!(
            store.search("database", 10).len(),
            0,
            "lexical search stays scoped to sess-B"
        );
        let hits = store.search_semantic(&embedder, "database outage", 5).await;
        assert!(
            hits.iter().any(|h| h.snippet.contains("database")),
            "semantic search should find the session-A memory: {hits:?}"
        );
    }

    #[tokio::test]
    async fn consolidate_merges_near_duplicates_keeping_the_newer_one() {
        let tmp = tempfile::tempdir().unwrap();
        let store = MemoryStore::open(&tmp.path().join("m.db")).unwrap();
        store.add(
            "tool_result",
            "bash",
            "database connection refused (attempt 1)",
        );
        store.add(
            "tool_result",
            "bash",
            "database connection refused (attempt 2)",
        );
        store.add("tool_result", "cargo", "unrelated rust note");

        let base = spawn_fake_embedder(4);
        let embedder = pirs_ai::EmbeddingClient::new(base, "mock", None);
        store.embed_pending(&embedder, 10).await;

        // Both "database" rows map to the identical fake vector (cosine ==
        // 1.0) so any threshold below that merges them; the rust row is
        // orthogonal and must survive untouched.
        let dry = store.consolidate(0.99, true);
        assert_eq!(dry, 1, "dry run should count exactly one merge");
        assert_eq!(
            store.search_all("database", 10).len(),
            2,
            "dry run must not have deleted anything yet"
        );

        let removed = store.consolidate(0.99, false);
        assert_eq!(removed, 1);
        let remaining = store.search_all("database", 10);
        assert_eq!(remaining.len(), 1, "the older duplicate should be gone");
        assert!(
            remaining[0].snippet.contains("attempt 2"),
            "the more recent of the pair must be the one kept: {remaining:?}"
        );
        assert_eq!(
            store.search_all("rust", 10).len(),
            1,
            "the unrelated memory must be untouched"
        );
    }
}
