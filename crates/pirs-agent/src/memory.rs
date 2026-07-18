//! Session memory as retrieval, not summarization. Tool results spill into a
//! SQLite FTS store as they happen; compaction demotes dropped messages into
//! the same store instead of destroying them. The `recall` tool searches it,
//! giving effectively unbounded sessions on small context windows.

use std::path::Path;
use std::sync::{Arc, Mutex};

use pirs_ai::Message;

pub struct MemoryStore {
    conn: Mutex<rusqlite::Connection>,
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
        conn.execute_batch(
            "CREATE VIRTUAL TABLE IF NOT EXISTS mem USING fts5(
                kind UNINDEXED, name, text, ts UNINDEXED
            );",
        )?;
        Ok(Arc::new(Self {
            conn: Mutex::new(conn),
        }))
    }

    pub fn add(&self, kind: &str, name: &str, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0);
        let _ = self.conn.lock().unwrap().execute(
            "INSERT INTO mem (kind, name, text, ts) VALUES (?1, ?2, ?3, ?4)",
            rusqlite::params![kind, name, text, ts],
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
                Message::ToolResult(r) => {
                    let text: String = r
                        .content
                        .iter()
                        .filter_map(|b| b.as_text())
                        .collect::<Vec<_>>()
                        .join("\n");
                    self.add("tool_result", &r.tool_name, &text);
                }
            }
        }
    }

    /// FTS search; returns up to `limit` hits ranked by relevance. The query
    /// is sanitized: each whitespace token becomes a quoted phrase, so user
    /// input containing FTS operators can't error or inject.
    pub fn search(&self, query: &str, limit: usize) -> Vec<MemoryHit> {
        let terms: Vec<String> = query
            .split_whitespace()
            .filter(|t| !t.is_empty())
            .map(|t| format!("\"{}\"", t.replace('"', "\"\"")))
            .collect();
        if terms.is_empty() {
            return Vec::new();
        }
        let match_q = terms.join(" ");
        let conn = self.conn.lock().unwrap();
        let mut stmt = match conn.prepare(
            "SELECT kind, name, snippet(mem, 2, '>>>', '<<<', '...', 24), ts
             FROM mem WHERE mem MATCH ?1 ORDER BY rank LIMIT ?2",
        ) {
            Ok(s) => s,
            Err(_) => return Vec::new(),
        };
        let rows = stmt.query_map(rusqlite::params![match_q, limit as i64], |r| {
            Ok(MemoryHit {
                kind: r.get(0)?,
                name: r.get(1)?,
                snippet: r.get(2)?,
                ts: r.get(3)?,
            })
        });
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
}
