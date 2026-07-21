//! pirs-claw — terminal agent covering Hermes-class gaps (minus Modal/Daytona/Singularity).
//!
//! - code + chat + schedule
//! - gateway: telegram / discord / slack / whatsapp / signal
//! - memory (FTS5), skills, pairing allowlist
//! - exec backends: local / docker / ssh
//! - voice transcription hook (external CLI)

pub mod attach;
pub mod channel;
pub mod cron_blueprints;
pub mod cron_util;
pub mod duration_parse;
pub mod exec_env;
pub mod gateway;
pub mod instance_lock;
pub mod learn;
pub mod life_tools;
pub mod memory_bridge;
pub mod pairing;
pub mod presets;
pub mod registry;
pub mod secrets;
pub mod session;
pub mod session_search;
pub mod skill_tools;
pub mod skills;
pub mod speech_setup;
pub mod voice;

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

pub use channel::{
    Channel, CliChannel, InboundMessage, OutboundReply, CHANNEL_CLI, CHANNEL_DISCORD,
    CHANNEL_SIGNAL, CHANNEL_SLACK, CHANNEL_TELEGRAM, CHANNEL_WHATSAPP, GATEWAY_CHANNELS,
};
pub use exec_env::{apply_exec_backend, describe_active as describe_exec_backend};
pub use pairing::{
    allow_all_enabled, warn_if_allow_all, PairingAllowlist, ALLOW_ALL_ENV, ALLOW_ALL_WARNING,
};
pub use presets::{
    apply_code_defaults, build_code_agent, coding_system_prompt, coding_tools, is_unattended,
    looks_like_repo, phase_scope_summary, resolve_code_strategy, unattended_forbidden_tool_names,
    unattended_tools, CodeOptions, DEFAULT_MODEL, DEFAULT_PLAN_MODEL, DEFAULT_STRATEGY,
    UNATTENDED_ENV,
};
pub use duration_parse::parse_duration_secs;
pub use secrets::{load_secrets_env, resolve_provider_and_key};
pub use session::{migrate_legacy_cli_session, SessionId, SessionLine, SessionMeta, SessionStore};
pub use pirs_skills::{
    default_skills_dir, discover_skills, find_skill, install_skill, install_skill_url, load_skills,
    remove_skill, skills_full_section, skills_prompt_section, usage_counts, validate_skill,
    validate_skill_name, write_skill, Skill,
};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
#[serde(rename_all = "snake_case")]
pub enum DeliverTarget {
    #[default]
    Cli,
    Telegram { chat_id: String },
    Discord { peer: String },
    Slack { peer: String },
    Whatsapp { peer: String },
    Signal { peer: String },
}

impl DeliverTarget {
    pub fn as_config_str(&self) -> String {
        match self {
            DeliverTarget::Cli => "cli".into(),
            DeliverTarget::Telegram { chat_id } => format!("telegram:{chat_id}"),
            DeliverTarget::Discord { peer } => format!("discord:{peer}"),
            DeliverTarget::Slack { peer } => format!("slack:{peer}"),
            DeliverTarget::Whatsapp { peer } => format!("whatsapp:{peer}"),
            DeliverTarget::Signal { peer } => format!("signal:{peer}"),
        }
    }

    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if let Some(id) = s.strip_prefix("telegram:") {
            DeliverTarget::Telegram {
                chat_id: id.to_string(),
            }
        } else if let Some(id) = s.strip_prefix("discord:") {
            DeliverTarget::Discord { peer: id.into() }
        } else if let Some(id) = s.strip_prefix("slack:") {
            DeliverTarget::Slack { peer: id.into() }
        } else if let Some(id) = s.strip_prefix("whatsapp:") {
            DeliverTarget::Whatsapp { peer: id.into() }
        } else if let Some(id) = s.strip_prefix("signal:") {
            DeliverTarget::Signal { peer: id.into() }
        } else {
            DeliverTarget::Cli
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ScheduleEntry {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub prompt: String,
    pub next_fire: u64,
    /// Interval recurrence (seconds). Ignored when `cron` is set. `0` = one-shot.
    pub every_secs: u64,
    /// Optional 5- or 6-field cron expression (Hermes-class schedules).
    /// When set, `every_secs` is ignored for next-fire calculation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    pub enabled: bool,
    #[serde(default)]
    pub deliver: DeliverTarget,
    /// Skill names to inject full body when the job fires.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Unix secs of last fire *attempt* (ok or error).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<u64>,
    /// `"ok"` | `"error"` after last attempt.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_status: Option<String>,
    /// Truncated error from last failed attempt (cleared on success).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Consecutive/total fail count (best-effort observability).
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub fail_count: u32,
}

fn is_zero_u32(n: &u32) -> bool {
    *n == 0
}

fn truncate_err(s: &str, max_chars: usize) -> String {
    let n = s.chars().count();
    if n <= max_chars {
        s.to_string()
    } else {
        s.chars().take(max_chars).collect::<String>() + "…"
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct ScheduleFile {
    jobs: Vec<ScheduleEntry>,
}

#[derive(Debug, Clone)]
pub struct ScheduleStore {
    path: PathBuf,
}

impl ScheduleStore {
    pub fn open(path: impl Into<PathBuf>) -> anyhow::Result<Self> {
        let path = path.into();
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        if !path.exists() {
            let empty = ScheduleFile::default();
            fs::write(&path, serde_json::to_string_pretty(&empty)?)?;
        }
        Ok(ScheduleStore { path })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn read(&self) -> anyhow::Result<ScheduleFile> {
        let text = fs::read_to_string(&self.path)?;
        if text.trim().is_empty() {
            return Ok(ScheduleFile::default());
        }
        match serde_json::from_str(&text) {
            Ok(f) => Ok(f),
            Err(e) => {
                // Never wipe jobs on parse failure — back up corrupt file and fail closed.
                let bak = self.path.with_extension("json.corrupt");
                let _ = fs::copy(&self.path, &bak);
                anyhow::bail!(
                    "schedule store corrupt at {}: {e} (backup written to {})",
                    self.path.display(),
                    bak.display()
                );
            }
        }
    }

    fn write(&self, f: &ScheduleFile) -> anyhow::Result<()> {
        // Atomic replace: write temp + fsync + rename so a crash mid-write
        // cannot leave a truncated schedule.json.
        let tmp = self.path.with_extension("json.tmp");
        let data = serde_json::to_string_pretty(f)?;
        fs::write(&tmp, &data)?;
        if let Ok(file) = fs::OpenOptions::new().write(true).open(&tmp) {
            let _ = file.sync_all();
        }
        fs::rename(&tmp, &self.path)?;
        Ok(())
    }

    pub fn list(&self) -> anyhow::Result<Vec<ScheduleEntry>> {
        Ok(self.read()?.jobs)
    }

    pub fn add(
        &self,
        prompt: &str,
        every_secs: u64,
        first_fire_in_secs: u64,
    ) -> anyhow::Result<ScheduleEntry> {
        self.add_with_deliver(prompt, every_secs, first_fire_in_secs, DeliverTarget::Cli)
    }

    pub fn add_with_deliver(
        &self,
        prompt: &str,
        every_secs: u64,
        first_fire_in_secs: u64,
        deliver: DeliverTarget,
    ) -> anyhow::Result<ScheduleEntry> {
        self.add_full(
            prompt,
            every_secs,
            first_fire_in_secs,
            deliver,
            None,
            Vec::new(),
            None,
        )
    }

    #[allow(clippy::too_many_arguments)]
    pub fn add_full(
        &self,
        prompt: &str,
        every_secs: u64,
        first_fire_in_secs: u64,
        deliver: DeliverTarget,
        name: Option<String>,
        skills: Vec<String>,
        model: Option<String>,
    ) -> anyhow::Result<ScheduleEntry> {
        self.add_full_cron(
            prompt,
            every_secs,
            first_fire_in_secs,
            None,
            deliver,
            name,
            skills,
            model,
        )
    }

    /// Add a job with optional cron expression (takes precedence over every_secs).
    #[allow(clippy::too_many_arguments)]
    pub fn add_full_cron(
        &self,
        prompt: &str,
        every_secs: u64,
        first_fire_in_secs: u64,
        cron: Option<String>,
        deliver: DeliverTarget,
        name: Option<String>,
        skills: Vec<String>,
        model: Option<String>,
    ) -> anyhow::Result<ScheduleEntry> {
        let mut f = self.read()?;
        let now = now_secs();
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let cron = match cron {
            Some(c) => Some(cron_util::normalize_cron_expr(&c)?),
            None => None,
        };
        let next_fire = if let Some(ref c) = cron {
            // First fire: next match after now (ignore first_fire_in unless >0 delay)
            let after = now.saturating_add(first_fire_in_secs);
            cron_util::next_fire_after(c, after)?
        } else {
            now.saturating_add(first_fire_in_secs)
        };
        let entry = ScheduleEntry {
            id: format!("job-{}-{}-{}", now, nanos, f.jobs.len()),
            name,
            prompt: prompt.into(),
            next_fire,
            every_secs: if cron.is_some() { 0 } else { every_secs },
            cron,
            enabled: true,
            deliver,
            skills,
            model,
            last_run: None,
            last_status: None,
            last_error: None,
            fail_count: 0,
        };
        f.jobs.push(entry.clone());
        self.write(&f)?;
        Ok(entry)
    }

    /// Resolve job by id or case-insensitive name.
    pub fn find(&self, id_or_name: &str) -> anyhow::Result<Option<ScheduleEntry>> {
        let f = self.read()?;
        Ok(f.jobs
            .into_iter()
            .find(|j| j.id == id_or_name || j.name.as_deref() == Some(id_or_name)))
    }

    pub fn set_enabled(&self, id_or_name: &str, enabled: bool) -> anyhow::Result<bool> {
        let mut f = self.read()?;
        let mut found = false;
        for j in &mut f.jobs {
            if j.id == id_or_name || j.name.as_deref() == Some(id_or_name) {
                j.enabled = enabled;
                if enabled {
                    // Resume: schedule next fire from now if past due.
                    let now = now_secs();
                    if j.next_fire < now {
                        j.next_fire = next_fire_for_job(j, now).unwrap_or(now + 60);
                    }
                }
                found = true;
                break;
            }
        }
        if found {
            self.write(&f)?;
        }
        Ok(found)
    }

    pub fn remove(&self, id_or_name: &str) -> anyhow::Result<bool> {
        let mut f = self.read()?;
        let before = f.jobs.len();
        f.jobs
            .retain(|j| j.id != id_or_name && j.name.as_deref() != Some(id_or_name));
        let removed = f.jobs.len() != before;
        if removed {
            self.write(&f)?;
        }
        Ok(removed)
    }

    pub fn due(&self, now: u64) -> anyhow::Result<Vec<ScheduleEntry>> {
        Ok(self
            .read()?
            .jobs
            .into_iter()
            .filter(|j| j.enabled && j.next_fire <= now)
            .collect())
    }

    pub fn mark_fired(&self, id: &str, now: u64) -> anyhow::Result<()> {
        let mut f = self.read()?;
        for j in &mut f.jobs {
            if j.id == id {
                j.last_run = Some(now);
                j.last_status = Some("ok".into());
                j.last_error = None;
                if j.cron.is_some() {
                    j.next_fire = next_fire_for_job(j, now).unwrap_or(now + 60);
                } else if j.every_secs == 0 {
                    j.enabled = false;
                } else {
                    j.next_fire = now.saturating_add(j.every_secs);
                }
            }
        }
        self.write(&f)?;
        Ok(())
    }

    /// Record a failed fire attempt without advancing `next_fire` (stays due for retry).
    pub fn mark_failed(&self, id: &str, now: u64, err: &str) -> anyhow::Result<()> {
        let mut f = self.read()?;
        for j in &mut f.jobs {
            if j.id == id {
                j.last_run = Some(now);
                j.last_status = Some("error".into());
                j.last_error = Some(truncate_err(err, 500));
                j.fail_count = j.fail_count.saturating_add(1);
            }
        }
        self.write(&f)?;
        Ok(())
    }

    /// Soonest enabled job by `next_fire`, if any.
    pub fn next_due(&self) -> anyhow::Result<Option<ScheduleEntry>> {
        let mut jobs: Vec<_> = self
            .read()?
            .jobs
            .into_iter()
            .filter(|j| j.enabled)
            .collect();
        jobs.sort_by_key(|j| j.next_fire);
        Ok(jobs.into_iter().next())
    }

    /// After long downtime, skip overdue recurring jobs to the next future fire
    /// instead of thundering-herd firing every missed slot. One-shots stay due.
    ///
    /// Policy: if `now - next_fire` exceeds `max(2 * every_secs, 3600)` (or 2h for
    /// cron), advance `next_fire` once and log via returned count.
    pub fn recover_missed(&self, now: u64) -> anyhow::Result<u32> {
        let mut f = self.read()?;
        let mut n = 0u32;
        for j in &mut f.jobs {
            if !j.enabled || j.next_fire >= now {
                continue;
            }
            let overdue = now.saturating_sub(j.next_fire);
            let threshold = if j.cron.is_some() {
                2 * 3600 // 2 hours for cron
            } else if j.every_secs > 0 {
                (j.every_secs.saturating_mul(2)).max(3600)
            } else {
                // one-shot: never skip
                continue;
            };
            if overdue <= threshold {
                continue;
            }
            match next_fire_for_job(j, now) {
                Ok(next) => {
                    j.next_fire = next;
                    j.last_error = Some(truncate_err(
                        &format!("skipped overdue fire ({overdue}s late); advanced next_fire"),
                        500,
                    ));
                    n += 1;
                }
                Err(_) => {}
            }
        }
        if n > 0 {
            self.write(&f)?;
        }
        Ok(n)
    }
}

/// Compute next fire for a job after `after` (unix secs).
fn next_fire_for_job(j: &ScheduleEntry, after: u64) -> anyhow::Result<u64> {
    if let Some(ref c) = j.cron {
        return cron_util::next_fire_after(c, after);
    }
    if j.every_secs == 0 {
        anyhow::bail!("one-shot job has no next fire");
    }
    Ok(after.saturating_add(j.every_secs.max(1)))
}

/// Parse `serve --channel` value: `all`, single name, or comma-separated list.
pub fn parse_channel_list(s: &str) -> anyhow::Result<Vec<String>> {
    let s = s.trim();
    if s.is_empty() {
        anyhow::bail!("empty channel list");
    }
    if s.eq_ignore_ascii_case("all") {
        return Ok(GATEWAY_CHANNELS.iter().map(|c| (*c).to_string()).collect());
    }
    let mut out = Vec::new();
    for part in s.split(',') {
        let p = part.trim().to_ascii_lowercase();
        if p.is_empty() {
            continue;
        }
        if !GATEWAY_CHANNELS.iter().any(|c| *c == p) {
            anyhow::bail!(
                "unknown channel {p:?}. Supported: {} (or all)",
                GATEWAY_CHANNELS.join(", ")
            );
        }
        if !out.contains(&p) {
            out.push(p);
        }
    }
    if out.is_empty() {
        anyhow::bail!("empty channel list");
    }
    Ok(out)
}

pub fn default_state_dir() -> PathBuf {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("claw")
}

pub fn default_session_path() -> PathBuf {
    default_state_dir().join("session.jsonl")
}

pub fn default_schedule_path() -> PathBuf {
    default_state_dir().join("schedule.json")
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn claw_system_prompt() -> String {
    let mut s = String::from(
        "You are pirs-claw, a personal assistant and coding agent.\n\
         Be helpful, concise, and honest. Use tools carefully for coding tasks.\n\
         You support chat, schedules, multi-channel gateway (telegram/discord/slack/whatsapp/signal), \
         skills under ~/.pirs/skills, and FTS memory.\n\
         When the user asks you to create, send, or share a file, use the attach_file tool so they \
         receive a real downloadable attachment — do not only paste the contents in chat.\n\
         Browser: browser_navigate / browser_screenshot. Vision: vision_describe on image paths. \
         Computer use (only if enabled): computer_screenshot / computer_click / computer_type.\n\
         Use session_search to recall past conversations across channels.\n",
    );
    s.push_str(&pirs_skills::soul_prompt_section());
    s
}

/// Gateway handler result: text reply plus optional file attachments.
#[derive(Debug, Clone, Default)]
pub struct GatewayReply {
    pub text: String,
    pub attachments: Vec<std::path::PathBuf>,
}

impl GatewayReply {
    pub fn text(s: impl Into<String>) -> Self {
        Self {
            text: s.into(),
            attachments: Vec::new(),
        }
    }

    pub fn with_attachments(mut self, paths: Vec<std::path::PathBuf>) -> Self {
        self.attachments = paths;
        self
    }
}

impl From<String> for GatewayReply {
    fn from(text: String) -> Self {
        Self::text(text)
    }
}

pub fn should_mark_schedule_fired(run: bool, fire_succeeded: bool) -> bool {
    run && fire_succeeded
}

pub fn require_llm_key(key: Option<&str>) -> anyhow::Result<()> {
    if key.map(|k| !k.trim().is_empty()).unwrap_or(false) {
        Ok(())
    } else {
        anyhow::bail!(
            "no API key for chat: set DASHSCOPE_API_KEY, DEEPSEEK_API_KEY, OPENROUTER_API_KEY, \
             or OPENAI_API_KEY (e.g. source ~/.pirs/secrets.env)"
        )
    }
}

pub fn extract_assistant_reply(msgs: &[pirs_ai::Message]) -> Option<String> {
    msgs.iter().rev().find_map(|m| match m {
        pirs_ai::Message::Assistant(a) => {
            let t = a.text();
            if t.trim().is_empty() {
                None
            } else {
                Some(t)
            }
        }
        _ => None,
    })
}

/// Diagnose why [`extract_assistant_reply`] returned `None` (for error messages).
pub fn empty_assistant_diag(msgs: &[pirs_ai::Message]) -> String {
    let n = msgs.len();
    let last = msgs.iter().rev().find_map(|m| match m {
        pirs_ai::Message::Assistant(a) => Some(a),
        _ => None,
    });
    match last {
        None => format!("no assistant message in turn ({n} message(s))"),
        Some(a) => {
            let text_len = a.text().len();
            let think_len: usize = a
                .content
                .iter()
                .filter_map(|b| match b {
                    pirs_ai::ContentBlock::Thinking { thinking, .. } => Some(thinking.len()),
                    _ => None,
                })
                .sum();
            let tools = a.tool_calls().len();
            format!(
                "last assistant: stop={:?} text_len={text_len} thinking_len={think_len} tool_calls={tools} err={:?}",
                a.stop_reason,
                a.error_message.as_deref()
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_survives_reopen() {
        let dir = tempfile::tempdir().unwrap();
        let s = SessionStore::open_for(dir.path(), SessionId::cli_local()).unwrap();
        s.append("user", "remember my dog is named Pixel").unwrap();
        s.append("assistant", "Got it — Pixel.").unwrap();
        let s2 = SessionStore::open_for(dir.path(), SessionId::cli_local()).unwrap();
        assert_eq!(s2.load().unwrap().len(), 2);
    }

    #[test]
    fn schedule_due_and_mark_fired_one_shot() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        let job = store.add("morning brief", 0, 0).unwrap();
        let now = now_secs() + 1;
        assert_eq!(store.due(now).unwrap().len(), 1);
        store.mark_fired(&job.id, now).unwrap();
        assert!(store.due(now + 10).unwrap().is_empty());
        let listed = store.list().unwrap();
        assert_eq!(listed[0].last_status.as_deref(), Some("ok"));
        assert!(listed[0].last_error.is_none());
    }

    #[test]
    fn schedule_mark_failed_keeps_due_and_records_error() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        let job = store.add("retry me", 0, 0).unwrap();
        let now = now_secs() + 1;
        store.mark_failed(&job.id, now, "boom").unwrap();
        let due = store.due(now).unwrap();
        assert_eq!(due.len(), 1);
        assert_eq!(due[0].last_status.as_deref(), Some("error"));
        assert_eq!(due[0].last_error.as_deref(), Some("boom"));
        assert_eq!(due[0].fail_count, 1);
    }

    #[test]
    fn schedule_corrupt_read_fails_closed() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("schedule.json");
        fs::write(&path, "{not json").unwrap();
        let store = ScheduleStore::open(&path).unwrap();
        let err = store.list().unwrap_err().to_string();
        assert!(err.contains("corrupt"), "{err}");
        assert!(path.with_extension("json.corrupt").exists());
    }

    #[test]
    fn schedule_atomic_write_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        store.add("a", 60, 0).unwrap();
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(!dir.path().join("schedule.json.tmp").exists());
    }

    #[test]
    fn schedule_recover_missed_skips_old_interval() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        let job = store.add("pulse", 60, 0).unwrap();
        // Force next_fire deep in the past.
        {
            let mut f = store.read().unwrap();
            f.jobs[0].next_fire = 1;
            store.write(&f).unwrap();
        }
        let now = now_secs();
        let n = store.recover_missed(now).unwrap();
        assert_eq!(n, 1);
        let j = store.find(&job.id).unwrap().unwrap();
        assert!(j.next_fire > now - 10, "next_fire should be near/future, got {}", j.next_fire);
        // One-shot overdue should not be skipped.
        let oneshot = store.add("once", 0, 0).unwrap();
        {
            let mut f = store.read().unwrap();
            for e in &mut f.jobs {
                if e.id == oneshot.id {
                    e.next_fire = 1;
                }
            }
            store.write(&f).unwrap();
        }
        assert_eq!(store.recover_missed(now).unwrap(), 0);
        assert!(store.due(now).unwrap().iter().any(|j| j.id == oneshot.id));
    }

    #[test]
    fn schedule_job_ids_unique_on_rapid_add() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        let a = store.add("a", 0, 0).unwrap();
        let b = store.add("b", 0, 0).unwrap();
        assert_ne!(a.id, b.id);
    }

    #[test]
    fn deliver_targets_parse() {
        assert_eq!(
            DeliverTarget::parse("telegram:1"),
            DeliverTarget::Telegram {
                chat_id: "1".into()
            }
        );
        assert_eq!(
            DeliverTarget::parse("slack:C01"),
            DeliverTarget::Slack {
                peer: "C01".into()
            }
        );
    }

    #[test]
    fn dry_run_tick_must_not_mark_fired() {
        assert!(!should_mark_schedule_fired(false, true));
        assert!(should_mark_schedule_fired(true, true));
    }

    #[test]
    fn require_llm_key_fails_closed() {
        assert!(require_llm_key(None).is_err());
        assert!(require_llm_key(Some("sk")).is_ok());
    }

    #[test]
    fn extract_reply() {
        assert!(extract_assistant_reply(&[]).is_none());
        let ok = pirs_ai::Message::Assistant(pirs_ai::AssistantMessage {
            content: vec![pirs_ai::ContentBlock::text("hello")],
            ..Default::default()
        });
        assert_eq!(extract_assistant_reply(&[ok]).as_deref(), Some("hello"));
    }

    #[test]
    fn extract_skips_thinking_only_assistant() {
        let only_think = pirs_ai::Message::Assistant(pirs_ai::AssistantMessage {
            content: vec![pirs_ai::ContentBlock::Thinking {
                thinking: "hmm".into(),
                thinking_signature: None,
                redacted: false,
            }],
            ..Default::default()
        });
        assert!(extract_assistant_reply(&[only_think.clone()]).is_none());
        let diag = empty_assistant_diag(&[only_think]);
        assert!(diag.contains("thinking_len=3"), "{diag}");
        assert!(diag.contains("text_len=0"), "{diag}");
    }

    #[test]
    fn schedule_pause_resume_remove() {
        let dir = tempfile::tempdir().unwrap();
        let store = ScheduleStore::open(dir.path().join("schedule.json")).unwrap();
        let job = store
            .add_full(
                "pulse",
                60,
                0,
                DeliverTarget::Cli,
                Some("morning".into()),
                vec!["skill-a".into()],
                None,
            )
            .unwrap();
        assert!(store.set_enabled("morning", false).unwrap());
        let now = now_secs() + 10;
        assert!(store.due(now).unwrap().is_empty());
        assert!(store.set_enabled(&job.id, true).unwrap());
        assert!(store.remove("morning").unwrap());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn parse_channel_list_all_and_csv() {
        let all = parse_channel_list("all").unwrap();
        assert!(all.contains(&"telegram".into()));
        assert_eq!(
            parse_channel_list("telegram,whatsapp").unwrap(),
            vec!["telegram".to_string(), "whatsapp".to_string()]
        );
        assert!(parse_channel_list("irc").is_err());
    }
}
