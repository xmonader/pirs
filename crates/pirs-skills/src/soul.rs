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
        if soul.is_file() { "present" } else { "missing — will use template" }
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
}
