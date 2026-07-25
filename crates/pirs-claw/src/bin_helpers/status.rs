//! status.rs
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::strategy::{run_strategy_async, PhaseReq, Task, ToolScope};
use pirs_agent::Agent;
use pirs_agent::AgentTool;
use pirs_claw::channel::{Channel, CliChannel, InboundMessage, OutboundReply, GATEWAY_CHANNELS};
use pirs_claw::memory_bridge;
use pirs_claw::pairing::PairingAllowlist;
use pirs_claw::presets::{
    apply_code_defaults, build_code_agent, coding_system_prompt, coding_tools, looks_like_repo,
    resolve_code_strategy, CodeOptions, DEFAULT_MODEL, DEFAULT_PLAN_MODEL, DEFAULT_STRATEGY,
};
use pirs_claw::registry;
use pirs_skills::{
    default_skills_dir, find_skill, install_skill, install_skill_url, load_skills, remove_skill,
    skill_tools, skills_full_section, skills_prompt_section, usage_counts, validate_skill, Skill,
};
use pirs_tools::life_tools;
use pirs_claw::parse_duration_secs;
use pirs_claw::{
    apply_exec_backend, claw_system_prompt, default_state_dir, describe_exec_backend,
    empty_assistant_diag, extract_assistant_reply, load_secrets_env, require_llm_key,
    should_mark_schedule_fired, DeliverTarget, GatewayReply, ScheduleStore, SessionId,
    SessionStore,
};

use super::tools::which_bin;

pub async fn print_runtime_status(state: &Path, schedule_path: &Path) -> anyhow::Result<()> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    println!("state_dir: {}", state.display());
    let pair = PairingAllowlist::default_path(state);
    let al = PairingAllowlist::open(&pair)?;
    println!("pairing: {} ({} peer(s))", pair.display(), al.list().len());
    for p in al.list() {
        println!("  - {p}");
    }
    let tg_token = std::env::var("TELEGRAM_BOT_TOKEN")
        .or_else(|_| std::env::var("PIRS_TELEGRAM_BOT_TOKEN"))
        .map(|t| !t.trim().is_empty())
        .unwrap_or(false);
    println!("telegram_token: {}", if tg_token { "set" } else { "missing" });
    println!(
        "telegram_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "telegram")
    );
    println!(
        "cron_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "cron")
    );
    let store = ScheduleStore::open(schedule_path)?;
    let jobs = store.list()?;
    println!("schedule: {} job(s) at {}", jobs.len(), schedule_path.display());
    if let Some(next) = store.next_due()? {
        let in_secs = next.next_fire.saturating_sub(now);
        println!(
            "  next_due: {} in {}s (next_fire={})",
            next.name.as_deref().unwrap_or(&next.id),
            in_secs,
            next.next_fire
        );
    }
    for j in jobs.iter().take(8) {
        println!(
            "  {} enabled={} cron={:?} every={} next={} last_run={:?} status={:?} fails={} err={:?}",
            j.name.as_deref().unwrap_or(&j.id),
            j.enabled,
            j.cron,
            j.every_secs,
            j.next_fire,
            j.last_run,
            j.last_status,
            j.fail_count,
            j.last_error.as_ref().map(|e| {
                if e.chars().count() > 80 {
                    format!("{}…", e.chars().take(80).collect::<String>())
                } else {
                    e.clone()
                }
            })
        );
    }
    if jobs.len() > 8 {
        println!("  … +{} more", jobs.len() - 8);
    }
    let sessions = state.join("sessions");
    let n_sess = if sessions.is_dir() {
        walkdir_sessions(&sessions).len()
    } else {
        0
    };
    println!("sessions: {n_sess} file(s) under {}", sessions.display());
    let cdp = std::env::var("PIRS_BROWSER_CDP_URL")
        .or_else(|_| std::env::var("BROWSER_CDP_URL"))
        .or_else(|_| std::env::var("CDP_URL"))
        .ok();
    let chrome_bin = ["chromium", "chromium-browser", "google-chrome", "google-chrome-stable", "chrome"]
        .into_iter()
        .find(|n| which_bin(n).is_some())
        .or_else(|| {
            if std::path::Path::new("/snap/bin/chromium").is_file() {
                Some("chromium")
            } else {
                None
            }
        });
    println!(
        "browser_cdp: {} chrome={}",
        cdp.as_deref().unwrap_or("(auto-launch or default :9222)"),
        chrome_bin.unwrap_or("missing")
    );
    println!("speech (probed):");
    for line in pirs_ai::speech_status_lines_probed().await {
        println!("  {line}");
    }
    println!(
        "tts_on_voice_default: {} (backends={})",
        pirs_claw::voice::tts_on_voice(),
        pirs_claw::voice::tts_backends_configured()
    );
    Ok(())
}



pub fn walkdir_sessions(root: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let Ok(channels) = std::fs::read_dir(root) else {
        return out;
    };
    for ch in channels.flatten() {
        if !ch.path().is_dir() {
            continue;
        }
        let channel = ch.file_name().to_string_lossy().into_owned();
        let Ok(peers) = std::fs::read_dir(ch.path()) else {
            continue;
        };
        for pe in peers.flatten() {
            let name = pe.file_name().to_string_lossy().into_owned();
            if !name.ends_with(".jsonl") {
                continue;
            }
            let peer = name.trim_end_matches(".jsonl");
            let meta_path = pe.path().with_file_name(format!("{peer}.meta.json"));
            let extra = std::fs::read_to_string(&meta_path)
                .ok()
                .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
                .map(|v| {
                    format!(
                        " msgs={} last={}",
                        v.get("message_count").and_then(|x| x.as_u64()).unwrap_or(0),
                        v.get("last_active").and_then(|x| x.as_u64()).unwrap_or(0)
                    )
                })
                .unwrap_or_default();
            out.push(format!("{channel}/{peer}{extra}"));
        }
    }
    out.sort();
    out
}



pub fn print_usage() {
    eprintln!(
        "pirs-claw — code + chat + schedule + gateway\n\
         \n\
         pirs-claw -C <repo> \"fix …\"\n\
         pirs-claw chat \"…\"\n\
         pirs-claw recall \"keyword\"\n\
         pirs-claw sessions\n\
         pirs-claw skills list|show|add|usage\n\
         pirs-claw schedule add --in 5m --every 1h \"…\"\n\
         pirs-claw schedule tick [--run]\n\
         pirs-claw serve --channel telegram|discord|slack|whatsapp|signal\n\
         pirs-claw pair list|add|remove\n\
         pirs-claw --exec docker|ssh:user@host …\n\
         \n\
         defaults: model={DEFAULT_MODEL} plan_model={DEFAULT_PLAN_MODEL} strategy={DEFAULT_STRATEGY}\n\
         exec backends: local, docker, docker:<image>, docker@ctr, ssh:user@host\n\
         (not supported: modal, daytona, singularity)\n\
         registry: ~/.pirs/config.toml + secrets.env (same as pirs)\n\
         gateway channels: {}",
        GATEWAY_CHANNELS.join(", ")
    );
}

