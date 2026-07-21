//! Cron blueprints + natural-language schedule parse (Hermes-class product).

use std::collections::HashMap;

use crate::cron_util;

#[derive(Debug, Clone)]
pub struct Blueprint {
    pub key: &'static str,
    pub title: &'static str,
    pub description: &'static str,
    /// 5-field cron template; may contain `{minute}` `{hour}` `{dow}` `{interval_min}`.
    pub schedule_template: &'static str,
    pub prompt_template: &'static str,
    pub default_slots: &'static [(&'static str, &'static str)],
}

pub const CATALOG: &[Blueprint] = &[
    Blueprint {
        key: "morning-brief",
        title: "Morning briefing",
        description: "Daily short briefing (calendar/weather/urgent items if available).",
        schedule_template: "{minute} {hour} * * *",
        prompt_template: "Produce a concise morning briefing: date, weather if known, open tasks, and anything urgent. Keep it scannable.",
        default_slots: &[("time", "08:00")],
    },
    Blueprint {
        key: "standup",
        title: "Weekday standup",
        description: "Weekday morning standup prompt.",
        schedule_template: "{minute} {hour} * * 1-5",
        prompt_template: "Generate a short weekday standup: yesterday, today, blockers. Be brief.",
        default_slots: &[("time", "09:00")],
    },
    Blueprint {
        key: "weekly-review",
        title: "Weekly review",
        description: "Weekly recap on a chosen weekday.",
        schedule_template: "{minute} {hour} * * {dow}",
        prompt_template: "Produce a weekly review: wins, open items, next week focus.",
        default_slots: &[("time", "17:00"), ("dow", "5")],
    },
    Blueprint {
        key: "heartbeat",
        title: "Heartbeat / pulse",
        description: "Recurring check-in every N minutes.",
        schedule_template: "*/{interval_min} * * * *",
        prompt_template: "Quick pulse: anything the user should know right now? If nothing, say 'all quiet'.",
        default_slots: &[("interval_min", "30")],
    },
    Blueprint {
        key: "eod",
        title: "End of day",
        description: "Evening wind-down summary.",
        schedule_template: "{minute} {hour} * * 1-5",
        prompt_template: "End-of-day wrap: what got done, what remains, one note for tomorrow.",
        default_slots: &[("time", "18:00")],
    },
];

pub fn list_blueprints() -> String {
    let mut out = String::from("Available schedule blueprints:\n");
    for b in CATALOG {
        out.push_str(&format!(
            "  {} — {}\n      {}\n      default: {:?}\n",
            b.key, b.title, b.description, b.default_slots
        ));
    }
    out.push_str(
        "\nUse: pirs-claw schedule add --blueprint morning-brief --name morning \"…optional override…\"\n",
    );
    out
}

pub fn find_blueprint(key: &str) -> Option<&'static Blueprint> {
    CATALOG.iter().find(|b| b.key == key || b.title.eq_ignore_ascii_case(key))
}

/// Expand a blueprint into (cron_expr, prompt) with optional slot overrides.
pub fn expand_blueprint(
    key: &str,
    slots: &HashMap<String, String>,
    prompt_override: Option<&str>,
) -> anyhow::Result<(String, String)> {
    let b = find_blueprint(key).ok_or_else(|| {
        anyhow::anyhow!("unknown blueprint {key:?}. Run: pirs-claw schedule blueprint list")
    })?;
    let mut map: HashMap<String, String> = b
        .default_slots
        .iter()
        .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
        .collect();
    for (k, v) in slots {
        map.insert(k.clone(), v.clone());
    }
    // Expand time HH:MM → hour/minute
    if let Some(t) = map.get("time").cloned() {
        let (h, m) = parse_hhmm(&t)?;
        map.insert("hour".into(), h.to_string());
        map.insert("minute".into(), m.to_string());
    }
    let mut cron = b.schedule_template.to_string();
    for (k, v) in &map {
        cron = cron.replace(&format!("{{{k}}}"), v);
    }
    if cron.contains('{') {
        anyhow::bail!("blueprint {key} missing slots for template {cron:?}");
    }
    let cron = cron_util::normalize_cron_expr(&cron)?;
    let prompt = prompt_override
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| b.prompt_template.to_string());
    Ok((cron, prompt))
}

fn parse_hhmm(s: &str) -> anyhow::Result<(u32, u32)> {
    let parts: Vec<_> = s.trim().split(':').collect();
    if parts.len() != 2 {
        anyhow::bail!("time must be HH:MM, got {s:?}");
    }
    let h: u32 = parts[0].parse()?;
    let m: u32 = parts[1].parse()?;
    if h > 23 || m > 59 {
        anyhow::bail!("invalid time {s:?}");
    }
    Ok((h, m))
}

/// Parse natural-language recurrence into a cron string or every_secs.
#[derive(Debug, Clone)]
pub enum NlSchedule {
    Cron(String),
    EverySecs(u64),
}

pub fn parse_nl_schedule(s: &str) -> anyhow::Result<NlSchedule> {
    let l = s.trim().to_ascii_lowercase();
    if l.is_empty() {
        anyhow::bail!("empty schedule");
    }
    // every N minutes/hours/days
    if let Some(rest) = l.strip_prefix("every ") {
        if let Some(n) = rest.strip_suffix(" minutes").or_else(|| rest.strip_suffix(" minute")) {
            let n: u64 = n.trim().parse()?;
            anyhow::ensure!((1..=24 * 60).contains(&n), "minutes out of range");
            return Ok(NlSchedule::Cron(format!("*/{n} * * * *")));
        }
        if let Some(n) = rest.strip_suffix(" hours").or_else(|| rest.strip_suffix(" hour")) {
            let n: u64 = n.trim().parse()?;
            anyhow::ensure!((1..=48).contains(&n), "hours out of range");
            return Ok(NlSchedule::EverySecs(n * 3600));
        }
        if rest == "day" || rest == "day at 9am" || rest.starts_with("day at ") {
            if let Some(t) = rest.strip_prefix("day at ") {
                let (h, m) = parse_ampm_or_hhmm(t)?;
                return Ok(NlSchedule::Cron(format!("{m} {h} * * *")));
            }
            return Ok(NlSchedule::Cron("0 9 * * *".into()));
        }
        if rest == "weekday" || rest.starts_with("weekday at ") {
            let t = rest.strip_prefix("weekday at ").unwrap_or("9:00");
            let (h, m) = parse_ampm_or_hhmm(t)?;
            return Ok(NlSchedule::Cron(format!("{m} {h} * * 1-5")));
        }
        if rest == "morning" {
            return Ok(NlSchedule::Cron("0 8 * * *".into()));
        }
        if rest == "evening" {
            return Ok(NlSchedule::Cron("0 18 * * 1-5".into()));
        }
    }
    if l.starts_with("daily at ") {
        let t = l.trim_start_matches("daily at ");
        let (h, m) = parse_ampm_or_hhmm(t)?;
        return Ok(NlSchedule::Cron(format!("{m} {h} * * *")));
    }
    if l.starts_with("weekdays at ") {
        let t = l.trim_start_matches("weekdays at ");
        let (h, m) = parse_ampm_or_hhmm(t)?;
        return Ok(NlSchedule::Cron(format!("{m} {h} * * 1-5")));
    }
    // raw cron pass-through
    if s.split_whitespace().count() >= 5 {
        let c = cron_util::normalize_cron_expr(s)?;
        return Ok(NlSchedule::Cron(c));
    }
    anyhow::bail!(
        "could not parse schedule {s:?}. Try: \"every 15 minutes\", \"weekdays at 9:00\", \
         \"daily at 8am\", or a cron expression"
    )
}

fn parse_ampm_or_hhmm(s: &str) -> anyhow::Result<(u32, u32)> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(t) = s.strip_suffix("am") {
        let t = t.trim();
        if t.contains(':') {
            let (h, m) = parse_hhmm(t)?;
            let h = if h == 12 { 0 } else { h };
            return Ok((h, m));
        }
        let h: u32 = t.parse()?;
        let h = if h == 12 { 0 } else { h };
        return Ok((h, 0));
    }
    if let Some(t) = s.strip_suffix("pm") {
        let t = t.trim();
        if t.contains(':') {
            let (h, m) = parse_hhmm(t)?;
            let h = if h == 12 { 12 } else { h + 12 };
            return Ok((h, m));
        }
        let h: u32 = t.parse()?;
        let h = if h == 12 { 12 } else { h + 12 };
        return Ok((h, 0));
    }
    parse_hhmm(&s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nl_every_15_min() {
        match parse_nl_schedule("every 15 minutes").unwrap() {
            NlSchedule::Cron(c) => assert!(c.contains("*/15")),
            _ => panic!("expected cron"),
        }
    }

    #[test]
    fn nl_weekdays() {
        match parse_nl_schedule("weekdays at 9:30").unwrap() {
            NlSchedule::Cron(c) => {
                assert!(c.contains("9"));
                assert!(c.contains("1-5"));
            }
            _ => panic!(),
        }
    }

    #[test]
    fn blueprint_morning() {
        let (cron, prompt) = expand_blueprint("morning-brief", &HashMap::new(), None).unwrap();
        assert!(cron.contains("8"));
        assert!(prompt.contains("briefing"));
    }
}
