//! Session memory as retrieval, not summarization. Tool results spill into a
//! SQLite FTS store as they happen; compaction demotes dropped messages into
//! the same store instead of destroying them. The `recall` tool searches it,
//! giving effectively unbounded sessions on small context windows.

use std::path::Path;
use std::sync::{Arc, Mutex};

use pirs_ai::Message;

pub struct MemoryStore {
    conn: Mutex<rusqlite::Connection>,
    /// Current session id; stamped on every row and used to scope `search`.
    /// Empty = unscoped (tests / embedded use).
    session: Mutex<String>,
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
            );",
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
}
