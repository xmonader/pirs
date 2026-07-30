//! In-process schedule ticker.
use std::path::PathBuf;
use std::time::Duration;

use super::outbound::deliver_outbound;

/// Background schedule runner used by the gateway daemon.
pub(super) async fn cron_ticker_loop(state_dir: PathBuf) {
    let schedule_path = state_dir.join("schedule.json");
    let mut interval = tokio::time::interval(Duration::from_secs(60));
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        interval.tick().await;
        // Non-blocking lock so overlapping ticks don't double-fire.
        let _lock = match crate::instance_lock::try_acquire(&state_dir, "cron") {
            Ok(l) => l,
            Err(_) => continue,
        };
        let store = match crate::ScheduleStore::open(&schedule_path) {
            Ok(s) => s,
            Err(e) => {
                eprintln!("[cron] open schedule: {e}");
                continue;
            }
        };
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        // Skip thundering-herd of long-overdue recurring jobs after downtime.
        match store.recover_missed(now) {
            Ok(n) if n > 0 => eprintln!("[cron] advanced {n} overdue job(s) past catch-up window"),
            Err(e) => eprintln!("[cron] recover_missed: {e}"),
            _ => {}
        }
        // Heartbeat (checklist file) — no hardware; optional soft prompt.
        if let Some(prompt) = pirs_skills::heartbeat_prompt(std::time::Duration::from_secs(
            pirs_skills::DEFAULT_MIN_INTERVAL_SECS,
        )) {
            eprintln!("[heartbeat] firing checklist turn");
            let mut cmd = std::process::Command::new(
                std::env::current_exe().unwrap_or_else(|_| "pirs-claw".into()),
            );
            cmd.arg("--state-dir")
                .arg(&state_dir)
                .env(crate::UNATTENDED_ENV, "1")
                .arg("chat")
                .arg(&prompt);
            match cmd.output() {
                Ok(out) if out.status.success() => {
                    let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    if !reply.is_empty() {
                        let _ = deliver_outbound(&crate::DeliverTarget::Cli, &reply).await;
                    }
                }
                Ok(out) => eprintln!(
                    "[heartbeat] chat exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                ),
                Err(e) => eprintln!("[heartbeat] spawn: {e}"),
            }
        }
        let due = match store.due(now) {
            Ok(d) => d,
            Err(e) => {
                eprintln!("[cron] due: {e}");
                continue;
            }
        };
        if due.is_empty() {
            continue;
        }
        let mut ok_n = 0u32;
        let mut fail_n = 0u32;
        for j in due {
            eprintln!(
                "[cron] due {} deliver={}: {}",
                j.id,
                j.deliver.as_config_str(),
                j.prompt
            );
            let mut cmd = std::process::Command::new(
                std::env::current_exe().unwrap_or_else(|_| "pirs-claw".into()),
            );
            cmd.arg("--state-dir")
                .arg(&state_dir)
                .env(crate::UNATTENDED_ENV, "1")
                .arg("chat");
            if let Some(ref m) = j.model {
                cmd.arg("--model").arg(m);
            }
            // Skill names are loaded by child via state; pass prompt only.
            // Attached skills: prefix into prompt for isolation.
            let prompt = if j.skills.is_empty() {
                j.prompt.clone()
            } else {
                format!(
                    "[scheduled job; skills: {}]\n{}",
                    j.skills.join(", "),
                    j.prompt
                )
            };
            cmd.arg(&prompt);
            let fire_result: Result<(), String> = match cmd.output() {
                Ok(out) if out.status.success() => {
                    let reply = String::from_utf8_lossy(&out.stdout).trim().to_string();
                    // Empty reply: still deliver a placeholder so users aren't left silent
                    // and we don't mark success with zero visible output on chat targets.
                    let text = if reply.is_empty() {
                        "(scheduled job finished with empty reply)".to_string()
                    } else {
                        reply
                    };
                    deliver_outbound(&j.deliver, &text)
                        .await
                        .map_err(|e| format!("deliver: {e}"))
                }
                Ok(out) => Err(format!(
                    "exit {:?}: {}",
                    out.status.code(),
                    String::from_utf8_lossy(&out.stderr)
                )),
                Err(e) => Err(format!("spawn: {e}")),
            };
            match fire_result {
                Ok(()) => {
                    let _ = store.mark_fired(&j.id, now);
                    ok_n += 1;
                }
                Err(err) => {
                    eprintln!("[cron] job {} failed: {err}", j.id);
                    let _ = store.mark_failed(&j.id, now, &err);
                    fail_n += 1;
                }
            }
        }
        eprintln!("[cron summary] ok={ok_n} failed={fail_n}");
    }
}
