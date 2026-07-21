//! Cron expression helpers for schedule jobs (Hermes-class recurring schedules).
//!
//! Accepts standard 5-field cron (`min hour dom mon dow`) or 6-field with seconds.
//! Uses the `cron` crate (seconds-first).

use std::str::FromStr;

use chrono::{DateTime, Local, TimeZone, Utc};
use cron::Schedule;

/// Normalize user cron to 6-field (sec min hour day month dow) for `cron` crate.
pub fn normalize_cron_expr(expr: &str) -> anyhow::Result<String> {
    let expr = expr.trim();
    if expr.is_empty() {
        anyhow::bail!("empty cron expression");
    }
    let parts: Vec<&str> = expr.split_whitespace().collect();
    let six = match parts.len() {
        5 => format!("0 {}", expr), // prepend seconds
        6 => expr.to_string(),
        7 => {
            // quartz-style with year — drop year
            parts[..6].join(" ")
        }
        n => anyhow::bail!(
            "cron expression must have 5 or 6 fields (got {n}): {expr:?}. \
             Examples: \"0 9 * * 1-5\" (weekdays 09:00), \"*/15 * * * *\" (every 15 min)"
        ),
    };
    // Validate parse
    Schedule::from_str(&six)
        .map_err(|e| anyhow::anyhow!("invalid cron {expr:?}: {e}"))?;
    Ok(six)
}

/// Next fire time as unix seconds (local timezone), strictly after `after_unix`.
pub fn next_fire_after(expr: &str, after_unix: u64) -> anyhow::Result<u64> {
    let six = normalize_cron_expr(expr)?;
    let schedule = Schedule::from_str(&six)
        .map_err(|e| anyhow::anyhow!("invalid cron: {e}"))?;
    let after = Utc
        .timestamp_opt(after_unix as i64, 0)
        .single()
        .ok_or_else(|| anyhow::anyhow!("bad timestamp {after_unix}"))?;
    // cron crate yields UTC; convert local wall-clock intent by using Local
    let after_local: DateTime<Local> = DateTime::from(after);
    let next = schedule
        .after(&after_local)
        .next()
        .ok_or_else(|| anyhow::anyhow!("cron {expr:?} has no next fire time"))?;
    Ok(next.timestamp() as u64)
}

/// Next fire from now (unix).
pub fn next_fire_now(expr: &str) -> anyhow::Result<u64> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    next_fire_after(expr, now)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_five_field() {
        let n = normalize_cron_expr("0 9 * * 1-5").unwrap();
        assert_eq!(n, "0 0 9 * * 1-5");
    }

    #[test]
    fn next_fire_is_in_future() {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        // every minute
        let n = next_fire_after("*/1 * * * *", now).unwrap();
        assert!(n > now, "n={n} now={now}");
        assert!(n - now <= 120);
    }

    #[test]
    fn rejects_garbage() {
        assert!(normalize_cron_expr("not a cron").is_err());
    }
}
