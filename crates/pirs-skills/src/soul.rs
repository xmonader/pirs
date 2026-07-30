//! User/soul profile — durable identity file Hermes-class agents maintain.
//!
//! Path: `~/.pirs/soul.md` (override with `PIRS_SOUL_PATH`).
//! Injected into system prompts; updated by the learning loop.

use std::fs;
use std::path::{Path, PathBuf};

pub fn default_soul_path() -> PathBuf {
    if let Ok(p) = std::env::var("PIRS_SOUL_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("soul.md")
}

pub fn read_soul() -> String {
    let path = default_soul_path();
    fs::read_to_string(&path).unwrap_or_else(|_| default_soul_template())
}

pub fn soul_prompt_section() -> String {
    let body = read_soul();
    if body.trim().is_empty() {
        return String::new();
    }
    format!(
        "\n\n## User profile (soul)\n\
         Durable facts about the user. Prefer these over guesses.\n\
         ---\n{}\n---\n",
        body.trim()
    )
}

/// Session-frozen identity block (Hermes volatile tier: built once per process
/// session, not re-read every turn — keeps prompt-cache prefixes stable).
///
/// Soul text is snapshotted on first capture. Mid-session soul writes (learn
/// loop) do **not** mutate this until [`invalidate_session_identity`].
#[derive(Debug, Clone, Default)]
pub struct SessionIdentitySnapshot {
    /// Frozen `## User profile (soul)` section (or empty).
    pub soul_section: String,
    /// Optional frozen memory digest (not query-specific).
    pub memory_section: String,
}

impl SessionIdentitySnapshot {
    /// Capture soul from disk once. Call at agent session start.
    pub fn capture() -> Self {
        Self {
            soul_section: soul_prompt_section(),
            memory_section: String::new(),
        }
    }

    /// Attach a pre-built memory digest (session-stable, not turn query).
    pub fn with_memory(mut self, memory_section: impl Into<String>) -> Self {
        self.memory_section = memory_section.into();
        self
    }

    /// Concatenated sections for the system prompt.
    pub fn prompt_sections(&self) -> String {
        let mut s = self.soul_section.clone();
        s.push_str(&self.memory_section);
        s
    }

    pub fn is_empty(&self) -> bool {
        self.soul_section.trim().is_empty() && self.memory_section.trim().is_empty()
    }
}

use std::sync::{Mutex, OnceLock};

static SESSION_IDENTITY: OnceLock<Mutex<Option<SessionIdentitySnapshot>>> = OnceLock::new();

fn session_identity_slot() -> &'static Mutex<Option<SessionIdentitySnapshot>> {
    SESSION_IDENTITY.get_or_init(|| Mutex::new(None))
}

/// Return the process-session identity snapshot, capturing soul on first call.
pub fn session_identity() -> SessionIdentitySnapshot {
    let slot = session_identity_slot();
    let mut guard = slot.lock().unwrap();
    if guard.is_none() {
        *guard = Some(SessionIdentitySnapshot::capture());
    }
    guard.clone().unwrap_or_default()
}

/// Replace / refresh the frozen identity (after soul curator edits, tests).
pub fn set_session_identity(snap: SessionIdentitySnapshot) {
    *session_identity_slot().lock().unwrap() = Some(snap);
}

/// Drop the frozen snapshot so the next [`session_identity`] re-reads soul.
pub fn invalidate_session_identity() {
    *session_identity_slot().lock().unwrap() = None;
}

/// Soul section for system prompts: process-session frozen (not re-read every turn).
pub fn session_soul_prompt_section() -> String {
    session_identity().soul_section
}

pub fn write_soul(body: &str) -> anyhow::Result<PathBuf> {
    let path = default_soul_path();
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    fs::write(&path, body)?;
    Ok(path)
}

pub fn default_soul_template() -> String {
    r#"# User soul / profile

## Identity
- (name, preferred address)

## Preferences
- (communication style, languages, timezone)

## Projects & context
- (active work, stack preferences)

## Constraints
- (things the agent must always / never do)

## Notes
- (free-form durable facts)
"#
    .into()
}

/// Normalize a fact line for duplicate detection.
fn normalize_fact_key(line: &str) -> String {
    line.to_ascii_lowercase()
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

const MAX_LEARNED_LINES: usize = 40;

/// Merge LLM-proposed bullet updates into soul.md (append under Notes if no structure match).
pub fn merge_soul_updates(current: &str, updates: &str) -> String {
    let updates = updates.trim();
    if updates.is_empty() || updates.eq_ignore_ascii_case("NOTHING") {
        return current.to_string();
    }
    let mut base = if current.trim().is_empty() {
        default_soul_template()
    } else {
        current.to_string()
    };
    if !base.ends_with('\n') {
        base.push('\n');
    }

    // Collect existing fact keys for stronger dedupe (not raw substring).
    let mut keys: std::collections::HashSet<String> = base
        .lines()
        .filter_map(|l| {
            let t = l.trim().trim_start_matches('-').trim();
            if t.is_empty() {
                None
            } else {
                Some(normalize_fact_key(t))
            }
        })
        .collect();

    // Cap growth of prior learned sections by not re-adding beyond limit.
    let existing_learned = base
        .lines()
        .filter(|l| l.trim_start().starts_with("- "))
        .count();

    let mut added = 0usize;
    let mut block = String::from("\n## Learned updates\n");
    for line in updates.lines() {
        let line = line.trim().trim_start_matches('-').trim();
        if line.is_empty() || line.eq_ignore_ascii_case("NOTHING") {
            continue;
        }
        let key = normalize_fact_key(line);
        if key.is_empty() || keys.contains(&key) {
            continue;
        }
        if existing_learned + added >= MAX_LEARNED_LINES {
            break;
        }
        keys.insert(key);
        block.push_str("- ");
        block.push_str(line);
        block.push('\n');
        added += 1;
    }
    if added == 0 {
        return base;
    }
    // Prefer a single Learned updates section: if one exists, append facts there.
    if let Some(idx) = base.find("\n## Learned updates\n") {
        let insert_at = idx + "\n## Learned updates\n".len();
        let facts: String = block
            .lines()
            .filter(|l| l.trim_start().starts_with("- "))
            .map(|l| format!("{l}\n"))
            .collect();
        base.insert_str(insert_at, &facts);
    } else {
        base.push_str(&block);
    }
    base
}

/// List installed skills with usage counts for curator CLI.
pub fn curator_report(skills_dir: &Path) -> String {
    use crate::skill::{load_skills, usage_counts};
    let skills = load_skills(skills_dir);
    let usage = usage_counts();
    let mut out = String::new();
    out.push_str(&format!("skills_dir: {}\n", skills_dir.display()));
    out.push_str(&format!("count: {}\n", skills.len()));
    for sk in skills {
        let u = usage.get(&sk.name).copied().unwrap_or(0);
        out.push_str(&format!(
            "- {}  uses={}  desc={}\n",
            sk.name,
            u,
            sk.description.chars().take(80).collect::<String>()
        ));
    }
    let soul = default_soul_path();
    out.push_str(&format!(
        "\nsoul: {} ({})\n",
        soul.display(),
        if soul.is_file() {
            "present"
        } else {
            "missing — will use template"
        }
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn merge_appends_unique() {
        let cur = "# User\n- likes rust\n";
        let m = merge_soul_updates(cur, "- timezone UTC+2\n- likes rust\n");
        assert!(m.contains("timezone UTC+2"));
        assert_eq!(m.matches("likes rust").count(), 1);
        // Second merge into same section does not duplicate.
        let m2 = merge_soul_updates(&m, "- timezone UTC+2\n");
        assert_eq!(m2.matches("timezone UTC+2").count(), 1);
    }

    #[test]
    fn session_identity_freezes_across_calls() {
        invalidate_session_identity();
        let a = session_identity();
        // Second capture must be the same frozen object, not a fresh re-read
        // that could diverge after a concurrent write.
        let b = session_identity();
        assert_eq!(a.soul_section, b.soul_section);
        invalidate_session_identity();
        let c = SessionIdentitySnapshot::capture().with_memory("\n## mem\n- fact\n");
        set_session_identity(c.clone());
        assert_eq!(session_identity().memory_section, c.memory_section);
        assert!(session_identity().prompt_sections().contains("fact"));
        invalidate_session_identity();
    }
}
