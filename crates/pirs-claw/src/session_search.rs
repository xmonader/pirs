//! Cross-session transcript search (Hermes-class "search past conversations").

use std::fs;
use std::path::{Path, PathBuf};

use serde::Serialize;

use crate::session::SessionLine;

#[derive(Debug, Clone, Serialize)]
pub struct SessionHit {
    pub session_key: String,
    pub path: String,
    pub role: String,
    pub snippet: String,
    pub score: u32,
    pub ts: u64,
}

/// Search `sessions/**/*.jsonl` under `state_dir` for query tokens.
///
/// When `peer_scope` is set (e.g. `telegram/12345` or `telegram/`), only sessions
/// whose key starts with that prefix are searched — prevents cross-peer leaks on
/// a shared gateway by default.
pub fn search_sessions(state_dir: &Path, query: &str, limit: usize) -> anyhow::Result<Vec<SessionHit>> {
    search_sessions_scoped(state_dir, query, limit, None)
}

/// Like [`search_sessions`] with optional peer/channel scope prefix.
pub fn search_sessions_scoped(
    state_dir: &Path,
    query: &str,
    limit: usize,
    peer_scope: Option<&str>,
) -> anyhow::Result<Vec<SessionHit>> {
    let root = state_dir.join("sessions");
    if !root.is_dir() {
        return Ok(Vec::new());
    }
    let tokens: Vec<String> = query
        .split_whitespace()
        .map(|t| t.to_ascii_lowercase())
        .filter(|t| t.len() >= 2)
        .collect();
    if tokens.is_empty() {
        anyhow::bail!("query too short");
    }
    let scope = peer_scope
        .map(|s| s.trim().trim_start_matches('/').to_string())
        .filter(|s| !s.is_empty());
    let mut hits = Vec::new();
    walk_jsonl(&root, &mut |path| {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let session_key = rel.trim_end_matches(".jsonl").replace('\\', "/");
        if let Some(ref sc) = scope {
            // Exact key or nested under `scope/` — never bare starts_with(scope),
            // which would let `telegram/1` match `telegram/10`.
            if !session_key_in_scope(&session_key, sc) {
                return;
            }
        }
        let Ok(text) = fs::read_to_string(path) else {
            return;
        };
        for line in text.lines() {
            if line.trim().is_empty() {
                continue;
            }
            let Ok(entry) = serde_json::from_str::<SessionLine>(line) else {
                continue;
            };
            let lower = entry.text.to_ascii_lowercase();
            let mut score = 0u32;
            for t in &tokens {
                if lower.contains(t) {
                    score = score.saturating_add(1 + t.len() as u32 / 4);
                }
            }
            if score == 0 {
                continue;
            }
            // Prefer user turns slightly for "what did I say"
            if entry.role == "user" {
                score = score.saturating_add(1);
            }
            hits.push(SessionHit {
                session_key: session_key.clone(),
                path: path.display().to_string(),
                role: entry.role,
                snippet: snippet(&entry.text, 200),
                score,
                ts: entry.ts,
            });
        }
    });
    hits.sort_by(|a, b| b.score.cmp(&a.score).then(b.ts.cmp(&a.ts)));
    hits.truncate(limit.max(1));
    Ok(hits)
}

fn snippet(s: &str, max: usize) -> String {
    let one: String = s.chars().map(|c| if c == '\n' { ' ' } else { c }).collect();
    if one.chars().count() <= max {
        one
    } else {
        format!("{}…", one.chars().take(max).collect::<String>())
    }
}

/// True if `session_key` is exactly `scope` or a path under `scope/`.
///
/// Deliberately does **not** use bare `starts_with(scope)` — that would admit
/// `telegram/10` when the caller is scoped to `telegram/1`.
pub fn session_key_in_scope(session_key: &str, scope: &str) -> bool {
    let scope = scope.trim().trim_start_matches('/').trim_end_matches('/');
    if scope.is_empty() {
        return true;
    }
    session_key == scope || session_key.starts_with(&format!("{scope}/"))
}

fn walk_jsonl(dir: &Path, f: &mut dyn FnMut(&Path)) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    for ent in rd.flatten() {
        let p = ent.path();
        if p.is_dir() {
            walk_jsonl(&p, f);
        } else if p.extension().and_then(|e| e.to_str()) == Some("jsonl") {
            f(&p);
        }
    }
}

/// Agent tool: search past gateway/CLI sessions (global — CLI / owner only).
///
/// Gateway message handling must use [`gateway_session_search_tool`] with the
/// inbound peer's `SessionId::key()` so other peers' transcripts never leak.
pub fn session_search_tool(state_dir: PathBuf) -> std::sync::Arc<dyn pirs_agent::AgentTool> {
    session_search_tool_scoped(state_dir, None)
}

/// Session search tool for a **specific** peer/channel key (`telegram/123`).
///
/// This is the only constructor the multi-tenant gateway may use. Scope is
/// stored on the tool instance (not a process-wide env var) so concurrent
/// handlers cannot clobber each other.
pub fn gateway_session_search_tool(
    state_dir: PathBuf,
    peer_session_key: &str,
) -> std::sync::Arc<dyn pirs_agent::AgentTool> {
    let key = peer_session_key.trim();
    debug_assert!(
        !key.is_empty(),
        "gateway session_search requires a non-empty peer session key"
    );
    session_search_tool_scoped(state_dir, Some(key.to_string()))
}

pub fn session_search_tool_scoped(
    state_dir: PathBuf,
    peer_scope: Option<String>,
) -> std::sync::Arc<dyn pirs_agent::AgentTool> {
    std::sync::Arc::new(SessionSearchTool {
        state_dir,
        peer_scope,
    })
}

struct SessionSearchTool {
    state_dir: PathBuf,
    peer_scope: Option<String>,
}

#[derive(serde::Deserialize, schemars::JsonSchema)]
struct SessionSearchArgs {
    /// Keywords to find in past conversation transcripts.
    query: String,
    #[serde(default = "default_limit")]
    limit: usize,
}

fn default_limit() -> usize {
    8
}

#[async_trait::async_trait]
impl pirs_agent::AgentTool for SessionSearchTool {
    fn name(&self) -> &str {
        "session_search"
    }

    fn description(&self) -> &str {
        if self.peer_scope.is_some() {
            "Search this peer's past conversation sessions by keyword."
        } else {
            "Search past conversation sessions by keyword (scoped to caller peer when set)."
        }
    }

    fn parameters(&self) -> serde_json::Value {
        serde_json::to_value(schemars::schema_for!(SessionSearchArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("session_search: full-text search past sessions")
    }

    async fn execute(
        &self,
        ctx: pirs_agent::ToolExecContext,
    ) -> anyhow::Result<pirs_agent::ToolOutput> {
        let args: SessionSearchArgs = serde_json::from_value(ctx.args)?;
        let hits = search_sessions_scoped(
            &self.state_dir,
            &args.query,
            args.limit.max(1),
            self.peer_scope.as_deref(),
        )?;
        if hits.is_empty() {
            return Ok(pirs_agent::ToolOutput::text(format!(
                "No session matches for {:?}",
                args.query
            )));
        }
        let mut out = String::new();
        for (i, h) in hits.iter().enumerate() {
            out.push_str(&format!(
                "{}. [{}] {} ({})\n   {}\n",
                i + 1,
                h.session_key,
                h.role,
                h.score,
                h.snippet
            ));
        }
        Ok(pirs_agent::ToolOutput::text(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session::{SessionId, SessionStore};

    #[test]
    fn finds_across_sessions() {
        let dir = tempfile::tempdir().unwrap();
        let a = SessionStore::open_for(dir.path(), SessionId::new("telegram", "1")).unwrap();
        let b = SessionStore::open_for(dir.path(), SessionId::new("cli", "local")).unwrap();
        a.append("user", "my dog is named Pixel").unwrap();
        b.append("user", "standup is at ten").unwrap();
        let hits = search_sessions(dir.path(), "pixel dog", 10).unwrap();
        assert!(!hits.is_empty());
        assert!(hits[0].snippet.to_ascii_lowercase().contains("pixel"));
    }

    #[test]
    fn peer_scope_hides_other_peers() {
        let dir = tempfile::tempdir().unwrap();
        let a = SessionStore::open_for(dir.path(), SessionId::new("telegram", "1")).unwrap();
        let b = SessionStore::open_for(dir.path(), SessionId::new("telegram", "2")).unwrap();
        a.append("user", "secret alpha token zebra").unwrap();
        b.append("user", "secret beta token zebra").unwrap();
        let scoped = search_sessions_scoped(dir.path(), "zebra secret", 10, Some("telegram/1"))
            .unwrap();
        assert!(!scoped.is_empty());
        assert!(
            scoped
                .iter()
                .all(|h| session_key_in_scope(&h.session_key, "telegram/1")),
            "scoped hits: {:?}",
            scoped.iter().map(|h| &h.session_key).collect::<Vec<_>>()
        );
        assert!(
            !scoped.iter().any(|h| h.snippet.contains("beta")),
            "must not return other peer: {scoped:?}"
        );
    }

    #[test]
    fn session_key_in_scope_rejects_numeric_prefix_collision() {
        assert!(session_key_in_scope("telegram/1", "telegram/1"));
        assert!(session_key_in_scope("telegram/1/extra", "telegram/1"));
        assert!(!session_key_in_scope("telegram/10", "telegram/1"));
        assert!(!session_key_in_scope("telegram/12", "telegram/1"));
        assert!(!session_key_in_scope("telegram/111", "telegram/1"));
        assert!(session_key_in_scope("telegram/10", "telegram/10"));
        // Channel-wide scope still matches all peers under that channel.
        assert!(session_key_in_scope("telegram/10", "telegram"));
        assert!(!session_key_in_scope("discord/1", "telegram"));
    }

    /// Regression: scope `telegram/1` must not admit session `telegram/10`.
    #[test]
    fn peer_1_scope_does_not_include_peer_10() {
        let dir = tempfile::tempdir().unwrap();
        let p1 = SessionStore::open_for(dir.path(), SessionId::new("telegram", "1")).unwrap();
        let p10 = SessionStore::open_for(dir.path(), SessionId::new("telegram", "10")).unwrap();
        p1.append("user", "marker-one-only confidential").unwrap();
        p10.append("user", "marker-ten-only confidential").unwrap();

        let scoped =
            search_sessions_scoped(dir.path(), "confidential marker", 20, Some("telegram/1"))
                .unwrap();
        assert!(
            !scoped.is_empty(),
            "peer 1 should have hits: {scoped:?}"
        );
        assert!(
            scoped.iter().all(|h| h.session_key == "telegram/1"
                || h.session_key.starts_with("telegram/1/")),
            "only telegram/1 keys: {:?}",
            scoped.iter().map(|h| &h.session_key).collect::<Vec<_>>()
        );
        assert!(
            !scoped.iter().any(|h| h.session_key == "telegram/10"
                || h.snippet.contains("marker-ten")),
            "telegram/1 scope must not include telegram/10: {scoped:?}"
        );

        let ten =
            search_sessions_scoped(dir.path(), "confidential marker", 20, Some("telegram/10"))
                .unwrap();
        assert!(
            ten.iter().any(|h| h.snippet.contains("marker-ten")),
            "peer 10 must still find its own hits: {ten:?}"
        );
        assert!(
            !ten.iter().any(|h| h.snippet.contains("marker-one")),
            "peer 10 must not see peer 1: {ten:?}"
        );
    }

    /// Gateway assembly path: tool built with inbound peer key must not leak
    /// another peer's transcripts. Unscoped tool *would* leak — proves scope
    /// is what prevents it.
    #[tokio::test]
    async fn gateway_tool_for_peer_cannot_return_other_peer_hits() {
        use pirs_agent::ToolExecContext;
        use tokio_util::sync::CancellationToken;

        let dir = tempfile::tempdir().unwrap();
        let peer_a = SessionId::new("telegram", "111");
        let peer_b = SessionId::new("telegram", "222");
        SessionStore::open_for(dir.path(), peer_a.clone())
            .unwrap()
            .append("user", "unique-alpha-marker confidential")
            .unwrap();
        SessionStore::open_for(dir.path(), peer_b.clone())
            .unwrap()
            .append("user", "unique-beta-marker confidential")
            .unwrap();

        // Same constructor the gateway uses for an inbound peer.
        let tool = gateway_session_search_tool(dir.path().to_path_buf(), &peer_a.key());
        assert_eq!(tool.name(), "session_search");
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t1".into(),
                args: serde_json::json!({"query": "confidential unique", "limit": 10}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.model_text().unwrap_or("");
        assert!(
            text.contains("unique-alpha-marker"),
            "caller peer hits expected: {text}"
        );
        assert!(
            !text.contains("unique-beta-marker"),
            "gateway tool for peer A must not leak peer B: {text}"
        );

        // Control: unscoped (CLI) tool sees both — proves data is present and
        // scoping is what filtered beta out.
        let global = session_search_tool(dir.path().to_path_buf());
        let global_out = global
            .execute(ToolExecContext {
                tool_call_id: "t2".into(),
                args: serde_json::json!({"query": "confidential unique", "limit": 10}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let gtext = global_out.model_text().unwrap_or("");
        assert!(
            gtext.contains("unique-beta-marker"),
            "unscoped tool must see other peer (control): {gtext}"
        );
    }

    #[test]
    fn gateway_message_handler_wires_peer_scoped_search() {
        // Structural: handle_gateway_message must pass sid.key() into tool build,
        // not bare session_search_tool(state) / env-only scope.
        let main_src = include_str!("main.rs");
        assert!(
            main_src.contains("gateway_session_search_tool"),
            "gateway tool assembly must use gateway_session_search_tool"
        );
        assert!(
            main_src.contains("sid.key().as_str()") || main_src.contains("sid.key()"),
            "gateway must pass inbound SessionId key as peer_scope"
        );
    }
}
