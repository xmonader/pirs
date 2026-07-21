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

/// Search all `sessions/**/*.jsonl` under `state_dir` for query tokens.
pub fn search_sessions(state_dir: &Path, query: &str, limit: usize) -> anyhow::Result<Vec<SessionHit>> {
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
    let mut hits = Vec::new();
    walk_jsonl(&root, &mut |path| {
        let rel = path
            .strip_prefix(&root)
            .unwrap_or(path)
            .to_string_lossy()
            .to_string();
        let session_key = rel.trim_end_matches(".jsonl").replace('\\', "/");
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
pub fn session_search_tool(state_dir: PathBuf) -> std::sync::Arc<dyn pirs_agent::AgentTool> {
    std::sync::Arc::new(SessionSearchTool { state_dir })
}

struct SessionSearchTool {
    state_dir: PathBuf,
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
        "Search past conversation sessions (all channels) by keyword. \
         Use when the user asks what was said earlier, across chats, or to recall prior context."
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
        let hits = search_sessions(&self.state_dir, &args.query, args.limit.max(1))?;
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
}
