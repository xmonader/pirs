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
            if !session_key.starts_with(sc.as_str())
                && !session_key.starts_with(&format!("{sc}/"))
            {
                // Also allow exact channel/peer prefix match.
                if !session_key.starts_with(sc) {
                    return;
                }
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

/// Agent tool: search past gateway/CLI sessions.
///
/// `peer_scope` defaults from `PIRS_CLAW_SESSION_PEER` when set (gateway sets this
/// per inbound peer). Without a scope, search is global (CLI / owner tools).
pub fn session_search_tool(state_dir: PathBuf) -> std::sync::Arc<dyn pirs_agent::AgentTool> {
    let peer_scope = std::env::var("PIRS_CLAW_SESSION_PEER").ok().filter(|s| !s.is_empty());
    session_search_tool_scoped(state_dir, peer_scope)
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
            scoped.iter().all(|h| h.session_key.starts_with("telegram/1")),
            "scoped hits: {:?}",
            scoped.iter().map(|h| &h.session_key).collect::<Vec<_>>()
        );
        assert!(
            !scoped.iter().any(|h| h.snippet.contains("beta")),
            "must not return other peer: {scoped:?}"
        );
    }
}
