//! Closed learning loop (Hermes-inspired, thin): memory facts + skill crystallize.

use std::path::Path;
use std::sync::Arc;

use pirs_ai::{CompletionOptions, Context, LlmProvider, Message, StreamEvent};

use pirs_agent::memory::MemoryStore;

use crate::skill::{default_skills_dir, validate_skill_name, write_skill};

/// Env: set to `1` to enable learn on gateway / always-on surfaces.
pub const LEARN_GATEWAY_ENV: &str = "PIRS_LEARN";
/// Env: set to `0`/`1` to disable learn on CLI one-shot.
pub const LEARN_DISABLE_ENV: &str = "PIRS_NO_LEARN";

fn env_truthy(key: &str) -> bool {
    matches!(
        std::env::var(key).as_deref(),
        Ok("1") | Ok("true") | Ok("yes")
    )
}

pub fn learn_enabled_cli() -> bool {
    // Back-compat claw env names.
    if env_truthy("PIRS_CLAW_NO_LEARN") || env_truthy(LEARN_DISABLE_ENV) {
        return false;
    }
    true
}

pub fn learn_enabled_gateway() -> bool {
    env_truthy(LEARN_GATEWAY_ENV) || env_truthy("PIRS_CLAW_LEARN")
}

/// Interactive TUI: default off (noise); opt-in with PIRS_LEARN=1.
pub fn learn_enabled_interactive() -> bool {
    env_truthy(LEARN_GATEWAY_ENV) || env_truthy("PIRS_LEARN_TUI")
}

const MEMORY_EXTRACT_PROMPT: &str = r#"Extract up to 3 durable personal facts or preferences from this exchange that would help a future assistant.
Output one fact per line, plain text. If nothing durable, output exactly: NOTHING
Do not invent facts. Prefer stable preferences, names, project conventions.

USER:
{user}

ASSISTANT:
{assistant}
"#;

const CRYSTALLIZE_PROMPT: &str = r#"Distill this completed session into a reusable skill.
Output ONLY the skill content as markdown with this exact frontmatter:
---
name: <short-kebab-name>
description: <one sentence: when should an agent use this skill>
---
<concise steps/gotchas worth reusing, max 30 lines>
If nothing generally reusable was learned (trivial task), output exactly: NOTHING

SESSION:
{transcript}
"#;

/// Heuristic: user text looks like something worth remembering.
pub fn looks_durable(user: &str) -> bool {
    let l = user.to_ascii_lowercase();
    l.contains("my name")
        || l.contains("i prefer")
        || l.contains("always ")
        || l.contains("never ")
        || l.contains("remember")
        || l.contains("call me")
        || l.contains("i work")
        || l.contains("timezone")
        || l.contains("my dog")
        || l.contains("my email")
}

/// Soft-fail memory extract → store as kind `fact`.
pub async fn maybe_memory_nudge(
    provider: Arc<dyn LlmProvider>,
    model: &str,
    api_key: Option<String>,
    state_dir: &Path,
    session_key: &str,
    user: &str,
    assistant: &str,
) {
    if !looks_durable(user) {
        return;
    }
    let prompt = MEMORY_EXTRACT_PROMPT
        .replace("{user}", &truncate(user, 2000))
        .replace("{assistant}", &truncate(assistant, 2000));
    let text = match complete_once(provider, model, api_key, &prompt).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[learn] memory nudge skipped: {e}");
            return;
        }
    };
    let text = text.trim().to_string();
    if text.is_empty() || text.eq_ignore_ascii_case("NOTHING") {
        return;
    }
    let Ok(mem) = MemoryStore::open(&state_dir.join("memory.db")) else {
        return;
    };
    mem.set_session(session_key);
    for line in text.lines() {
        let line = line.trim().trim_start_matches('-').trim();
        if line.is_empty() || line.eq_ignore_ascii_case("NOTHING") {
            continue;
        }
        mem.add("fact", "learn", line);
        eprintln!("[learn] remembered: {line}");
    }
}

/// Crystallize a skill from a transcript when substantial work happened.
pub async fn maybe_crystallize_skill(
    provider: Arc<dyn LlmProvider>,
    model: &str,
    api_key: Option<String>,
    transcript: &str,
    min_chars: usize,
) -> Option<std::path::PathBuf> {
    if transcript.chars().count() < min_chars {
        return None;
    }
    let prompt = CRYSTALLIZE_PROMPT.replace("{transcript}", &truncate(transcript, 6000));
    let text = match complete_once(provider, model, api_key, &prompt).await {
        Ok(t) => t,
        Err(e) => {
            eprintln!("[learn] crystallize skipped: {e}");
            return None;
        }
    };
    let text = text.trim().to_string();
    if text.is_empty() || text.starts_with("NOTHING") || !text.starts_with("---") {
        return None;
    }
    // Parse name/description from frontmatter lightly.
    let (name, description, body) = match parse_crystallized(&text) {
        Some(x) => x,
        None => {
            eprintln!("[learn] crystallize: could not parse skill frontmatter");
            return None;
        }
    };
    if validate_skill_name(&name).is_err() {
        eprintln!("[learn] crystallize: invalid skill name {name:?}");
        return None;
    }
    match write_skill(&default_skills_dir(), &name, &description, &body) {
        Ok(p) => {
            eprintln!("[learn] crystallized skill → {}", p.display());
            Some(p)
        }
        Err(e) => {
            eprintln!("[learn] crystallize write failed: {e}");
            None
        }
    }
}

fn parse_crystallized(raw: &str) -> Option<(String, String, String)> {
    let rest = raw.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let fm = &rest[..end];
    let body = rest[end + 4..].trim().to_string();
    let mut name = String::new();
    let mut description = String::new();
    for line in fm.lines() {
        let line = line.trim();
        if let Some(v) = line.strip_prefix("name:") {
            name = v.trim().trim_matches('"').to_string();
        } else if let Some(v) = line.strip_prefix("description:") {
            description = v.trim().trim_matches('"').to_string();
        }
    }
    if name.is_empty() || description.is_empty() {
        return None;
    }
    Some((name, description, body))
}

async fn complete_once(
    provider: Arc<dyn LlmProvider>,
    model: &str,
    api_key: Option<String>,
    prompt: &str,
) -> anyhow::Result<String> {
    let opts = CompletionOptions {
        api_key,
        max_tokens: Some(800),
        ..Default::default()
    };
    let ctx = Context {
        system_prompt: Some(
            "You extract durable facts or write skill documents. Be concise.".into(),
        ),
        messages: vec![Message::user(prompt)],
        tools: Vec::new(),
    };
    let stream = provider
        .stream(
            model,
            &ctx,
            &opts,
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    use futures::StreamExt;
    let mut text = String::new();
    let mut stream = std::pin::pin!(stream);
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(d) => text.push_str(&d),
            StreamEvent::Done(msg) => {
                let t = msg.text();
                if !t.is_empty() {
                    text = t;
                }
                break;
            }
            StreamEvent::Error(message) => {
                anyhow::bail!("{message}");
            }
            _ => {}
        }
    }
    Ok(text)
}

fn truncate(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        s.chars().take(max).collect()
    }
}

/// Build a short transcript from user + assistant (+ optional tool notes).
pub fn session_transcript(user: &str, assistant: &str, extra: &str) -> String {
    format!(
        "USER:\n{}\n\nASSISTANT:\n{}\n\n{}",
        truncate(user, 3000),
        truncate(assistant, 3000),
        truncate(extra, 2000)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn durable_heuristic() {
        assert!(looks_durable("Remember my dog is Pixel"));
        assert!(!looks_durable("what time is it"));
    }

    #[test]
    fn parse_crystallized_ok() {
        let raw = "---\nname: deploy-app\ndescription: when deploying\n---\n\n1. run tests\n";
        let (n, d, b) = parse_crystallized(raw).unwrap();
        assert_eq!(n, "deploy-app");
        assert_eq!(d, "when deploying");
        assert!(b.contains("run tests"));
    }
}
