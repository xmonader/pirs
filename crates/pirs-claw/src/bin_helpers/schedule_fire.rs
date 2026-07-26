//! schedule_fire.rs
use std::path::Path;

use pirs_claw::registry;
use pirs_skills::{
    skills_full_section, Skill,
};
use pirs_claw::require_llm_key;


pub async fn fire_schedule_job(
    job: &pirs_claw::ScheduleEntry,
    state: &Path,
    default_model: &str,
    all_skills: &[Skill],
) -> anyhow::Result<bool> {
    // Fail early with a clear message (schedule fires always need an LLM key).
    let key = match registry::resolve_llm(default_model, 1) {
        Ok((_provider, key, _routed)) => key,
        Err(e) => anyhow::bail!("schedule job {}: cannot resolve model/key: {e}", job.id),
    };
    if let Err(e) = require_llm_key(key.as_deref()) {
        anyhow::bail!("schedule job {}: {e}", job.id);
    }
    let model = job.model.as_deref().unwrap_or(default_model);
    let attached = pirs_claw::skills::select_skills(all_skills, &job.skills);
    let prompt = if attached.is_empty() {
        job.prompt.clone()
    } else {
        format!(
            "{}\n\n{}",
            skills_full_section(&attached),
            job.prompt
        )
    };
    // Isolated job chat: use a temp state subdir so schedule doesn't pollute cli/local.
    let job_state = state.join("schedule-runs").join(&job.id);
    std::fs::create_dir_all(&job_state)?;
    // Timeout so a hung child cannot hold the cron flock forever (M-28).
    let timeout_secs: u64 = std::env::var("PIRS_CLAW_SCHEDULE_TIMEOUT_SECS")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(600)
        .clamp(30, 7200);
    let mut child = tokio::process::Command::new(std::env::current_exe()?)
        .arg("--model")
        .arg(model)
        .arg("--state-dir")
        .arg(&job_state)
        .arg("--no-learn")
        .env(pirs_claw::UNATTENDED_ENV, "1")
        .arg("chat")
        .arg(&prompt)
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()?;
    let stdout = child.stdout.take();
    let stderr = child.stderr.take();
    let out_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut s = Vec::new();
        if let Some(mut r) = stdout {
            let _ = r.read_to_end(&mut s).await;
        }
        s
    });
    let err_task = tokio::spawn(async move {
        use tokio::io::AsyncReadExt;
        let mut s = Vec::new();
        if let Some(mut r) = stderr {
            let _ = r.read_to_end(&mut s).await;
        }
        s
    });
    let status = match tokio::time::timeout(
        std::time::Duration::from_secs(timeout_secs),
        child.wait(),
    )
    .await
    {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => {
            eprintln!("[tick] job {} wait error: {e}", job.id);
            out_task.abort();
            err_task.abort();
            return Ok(false);
        }
        Err(_) => {
            let _ = child.start_kill();
            let _ = child.wait().await;
            out_task.abort();
            err_task.abort();
            eprintln!(
                "[tick] job {} timed out after {timeout_secs}s",
                job.id
            );
            return Ok(false);
        }
    };
    let stdout = out_task.await.unwrap_or_default();
    let stderr = err_task.await.unwrap_or_default();
    if !status.success() {
        eprintln!(
            "[tick] job {} chat failed: {}",
            job.id,
            String::from_utf8_lossy(&stderr)
        );
        return Ok(false);
    }
    let reply = String::from_utf8_lossy(&stdout).trim().to_string();
    let text = if reply.is_empty() {
        "(scheduled job finished with empty reply)".to_string()
    } else {
        reply
    };
    if let Err(e) = pirs_claw::gateway::deliver_outbound(&job.deliver, &text).await {
        eprintln!(
            "[tick] job {} deliver {} failed: {e}",
            job.id,
            job.deliver.as_config_str()
        );
        return Ok(false);
    }
    Ok(true)
}

