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
/// Built from recent durable rows (`fact` / user chat) via `recent_hits`, not a
/// fake FTS query, so the system prefix stays byte-stable and useful.
pub fn session_memory_digest(store: &MemoryStore, limit: usize) -> String {
    let limit = limit.max(1);
    let pool = store.recent_hits(limit.saturating_mul(4).max(8), false);
    if pool.is_empty() {
        return String::new();
    }
    // Prefer durable kinds; fall back to any recent row.
    let mut preferred: Vec<pirs_agent::MemoryHit> = pool
        .iter()
        .filter(|h| {
            matches!(
                h.kind.as_str(),
                "fact" | "user" | "chat" | "preference" | "note"
            )
        })
        .cloned()
        .collect();
    if preferred.is_empty() {
        preferred = pool;
    }
    // Dedupe by normalized snippet prefix, keep newest-first order from recent_hits.
    let mut seen = std::collections::HashSet::new();
    preferred.retain(|h| {
        let key = h
            .snippet
            .chars()
            .take(80)
            .collect::<String>()
            .to_ascii_lowercase();
        seen.insert(key)
    });
    preferred.truncate(limit);
    // Truncate long snippets for the system prompt.
    for h in &mut preferred {
        if h.snippet.chars().count() > 160 {
            h.snippet = format!("{}…", h.snippet.chars().take(160).collect::<String>());
        }
    }
    format_digest(&preferred)
}

fn format_digest(hits: &[pirs_agent::MemoryHit]) -> String {
    if hits.is_empty() {
        return String::new();
    }
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

    #[test]
    fn session_memory_digest_uses_recent_not_fake_query() {
        let dir = tempfile::tempdir().unwrap();
        let store = open_memory(dir.path()).unwrap();
        scope_session(&store, "cli/local");
        store.add("fact", "pref", "user prefers rust and short answers");
        store.add("tool_result", "bash", "noise from tools should rank lower");
        let digest = session_memory_digest(&store, 3);
        assert!(
            digest.contains("session snapshot"),
            "system section header: {digest}"
        );
        assert!(
            digest.contains("prefers rust") || digest.contains("fact"),
            "durable fact preferred: {digest}"
        );
        // Stable re-call (same recent rows) — not empty.
        let again = session_memory_digest(&store, 3);
        assert_eq!(digest, again, "digest must be stable for same store state");
    }
}
