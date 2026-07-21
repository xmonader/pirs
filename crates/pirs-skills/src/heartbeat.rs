//! Heartbeat-style periodic agent turns (checklist file, not hardware).
//!
//! Reads `~/.pirs/heartbeat.md` (or `PIRS_HEARTBEAT_PATH`) and returns a prompt
//! when the minimum interval has elapsed since the last fire.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Default minimum interval between heartbeats (30 minutes).
pub const DEFAULT_MIN_INTERVAL_SECS: u64 = 30 * 60;

pub fn heartbeat_path() -> PathBuf {
    if let Ok(p) = std::env::var("PIRS_HEARTBEAT_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("heartbeat.md")
}

fn stamp_path() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home)
        .join(".pirs")
        .join("heartbeat.last")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Whether enough time has passed since the last heartbeat.
pub fn due(min_interval: Duration) -> bool {
    let stamp = stamp_path();
    let last = std::fs::read_to_string(&stamp)
        .ok()
        .and_then(|s| s.trim().parse::<u64>().ok())
        .unwrap_or(0);
    now_secs().saturating_sub(last) >= min_interval.as_secs().max(1)
}

/// Mark heartbeat as fired now.
pub fn mark_fired() {
    let p = stamp_path();
    if let Some(parent) = p.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let _ = std::fs::write(p, format!("{}", now_secs()));
}

/// Read checklist body if the file exists.
pub fn read_checklist() -> Option<String> {
    let p = heartbeat_path();
    let body = std::fs::read_to_string(&p).ok()?;
    let t = body.trim();
    if t.is_empty() {
        None
    } else {
        Some(t.to_string())
    }
}

/// Build a user prompt for a heartbeat turn, or None if not due / no checklist.
pub fn maybe_prompt(min_interval: Duration) -> Option<String> {
    if !due(min_interval) {
        return None;
    }
    let checklist = read_checklist()?;
    mark_fired();
    Some(format!(
        "[heartbeat] Periodic check-in. Work through this checklist briefly \
         (no long history). Report only what needs attention:\n\n{checklist}"
    ))
}

/// Ensure a default checklist template exists.
pub fn ensure_template() -> std::io::Result<PathBuf> {
    let p = heartbeat_path();
    if !p.is_file() {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(
            &p,
            "# Heartbeat checklist\n\n\
             - [ ] Any failing schedules?\n\
             - [ ] Unread high-priority notes?\n\
             - [ ] Project tests still green?\n",
        )?;
    }
    Ok(p)
}

/// Pure helper for tests: due given last stamp and now.
pub fn due_at(last_secs: u64, now_secs: u64, min_interval: Duration) -> bool {
    now_secs.saturating_sub(last_secs) >= min_interval.as_secs().max(1)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn due_at_respects_interval() {
        assert!(!due_at(100, 150, Duration::from_secs(120)));
        assert!(due_at(100, 250, Duration::from_secs(120)));
    }

    #[test]
    fn ensure_template_and_prompt_path() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let p = ensure_template().unwrap();
        assert!(p.is_file());
        // Force due by clearing stamp
        let _ = std::fs::remove_file(stamp_path());
        let prompt = maybe_prompt(Duration::from_secs(1)).unwrap();
        assert!(prompt.contains("[heartbeat]"));
        assert!(prompt.contains("checklist") || prompt.contains("failing"));
        // Immediately not due again
        assert!(maybe_prompt(Duration::from_secs(3600)).is_none());
    }
}
