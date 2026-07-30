//! status.rs
use std::path::Path;

use pirs_claw::channel::GATEWAY_CHANNELS;
use pirs_claw::pairing::PairingAllowlist;
use pirs_claw::presets::{DEFAULT_MODEL, DEFAULT_PLAN_MODEL, DEFAULT_STRATEGY};
use pirs_claw::ScheduleStore;

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
    println!(
        "telegram_token: {}",
        if tg_token { "set" } else { "missing" }
    );
    println!(
        "telegram_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "telegram")
    );
    println!(
        "cron_lock: {}",
        pirs_claw::instance_lock::lock_status(state, "cron")
    );
    let store = ScheduleStore::open(schedule_path)?;
    for line in store.status_lines(now)? {
        println!("{line}");
    }
    // Extra detail for top jobs (cron/every/error tail) beyond compact status_lines.
    for j in store.list()?.iter().take(8) {
        if j.last_error.is_some() || j.cron.is_some() {
            println!(
                "  detail {} cron={:?} every={} err={:?}",
                j.name.as_deref().unwrap_or(&j.id),
                j.cron,
                j.every_secs,
                j.last_error.as_ref().map(|e| {
                    if e.chars().count() > 80 {
                        format!("{}…", e.chars().take(80).collect::<String>())
                    } else {
                        e.clone()
                    }
                })
            );
        }
    }
    println!("channels: telegram=spine; discord/slack/whatsapp/signal=stub/thin");
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
    let chrome_bin = [
        "chromium",
        "chromium-browser",
        "google-chrome",
        "google-chrome-stable",
        "chrome",
    ]
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
