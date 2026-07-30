//! Wire pirs-agent FTS5 memory into claw sessions (Hermes memory gap).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pirs_agent::memory::MemoryStore;

/// Open claw memory DB under state dir.
pub fn open_memory(state_dir: &Path) -> anyhow::Result<Arc<MemoryStore>> {
    let path = state_dir.join("memory.db");
    MemoryStore::open(&path).map_err(|e| anyhow::anyhow!("memory open: {e}"))
}

/// Scope memory rows to a session key (channel/peer).
pub fn scope_session(store: &MemoryStore, session_key: &str) {
    store.set_session(session_key);
}

/// Persist a chat turn for later recall.
pub fn remember_turn(store: &MemoryStore, role: &str, text: &str) {
    store.add(role, "chat", text);
}

/// Keyword recall snippet for system prompt (top hits).
///
/// Prefer [`session_memory_digest`] for the system prompt (session-stable) and
/// [`turn_recall_for_user`] for query-specific hits on the **user** message
/// (Hermes: never mutate system mid-session for turn prefetch).
pub fn recall_context(store: &MemoryStore, query: &str, limit: usize) -> String {
    let hits = store.search(query, limit);
    if hits.is_empty() {
        return String::new();
    }
    let mut s = String::from("\n\n## Memory recall\n");
    for h in hits {
        s.push_str(&format!("- [{}] {}: {}\n", h.kind, h.name, h.snippet));
    }
    s
}

/// Session-stable memory digest for the system prompt (not query-matched).
///
/// Built once per agent session from recent durable rows so the system prefix
/// stays byte-stable across turns (prompt-cache friendly).
pub fn session_memory_digest(store: &MemoryStore, limit: usize) -> String {
    // Prefer fact-kind / durable rows via a broad search on common anchors.
    let hits = store.search("prefer remember always name project timezone", limit.max(1));
    if hits.is_empty() {
        // Fall back to any recent keyword that yields something.
        let hits = store.search("the a is", limit.max(1));
        if hits.is_empty() {
            return String::new();
        }
        return format_digest(&hits);
    }
    format_digest(&hits)
}

fn format_digest(hits: &[pirs_agent::MemoryHit]) -> String {
    let mut s = String::from(
        "\n\n## Memory (session snapshot)\n\
         Durable facts frozen at session start. Prefer these over guesses.\n",
    );
    for h in hits {
        s.push_str(&format!("- [{}] {}: {}\n", h.kind, h.name, h.snippet));
    }
    s
}

/// Query-specific recall for injection into the **current user** message only.
pub fn turn_recall_for_user(store: &MemoryStore, query: &str, limit: usize) -> String {
    let hits = store.search(query, limit);
    if hits.is_empty() {
        return String::new();
    }
    let mut s = String::from("[memory for this turn]\n");
    for h in hits {
        s.push_str(&format!("- [{}] {}: {}\n", h.kind, h.name, h.snippet));
    }
    s.push('\n');
    s
}

/// Wrap a user turn with optional turn-local memory prefetch (Hermes user envelope).
pub fn user_text_with_turn_recall(
    store: Option<&MemoryStore>,
    query: &str,
    limit: usize,
) -> String {
    match store {
        Some(m) => {
            let prefix = turn_recall_for_user(m, query, limit);
            if prefix.is_empty() {
                query.to_string()
            } else {
                format!("{prefix}{query}")
            }
        }
        None => query.to_string(),
    }
}

pub fn memory_db_path(state_dir: &Path) -> PathBuf {
    state_dir.join("memory.db")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn remember_and_recall() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_memory(dir.path()).unwrap();
        scope_session(&store, "cli/local");
        remember_turn(&store, "user", "my dog is named Pixel");
        remember_turn(&store, "assistant", "Got it about Pixel");
        let ctx = recall_context(&store, "Pixel", 5);
        assert!(ctx.contains("Pixel"), "{ctx}");
    }

    #[test]
    fn turn_recall_for_user_is_prefix_not_system_section() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_memory(dir.path()).unwrap();
        scope_session(&store, "cli/local");
        remember_turn(&store, "user", "I prefer rust forever");
        let wrapped = user_text_with_turn_recall(Some(&store), "what next?", 5);
        assert!(
            wrapped.contains("what next?"),
            "user text preserved: {wrapped}"
        );
        // Either with or without hits depending on FTS tokenization; if hits,
        // they must be a turn prefix, not a ## Memory system section.
        if wrapped != "what next?" {
            assert!(
                wrapped.starts_with("[memory for this turn]"),
                "turn recall is user envelope: {wrapped}"
            );
            assert!(
                !wrapped.contains("## Memory recall"),
                "must not use system section format: {wrapped}"
            );
        }
    }
}
