//! Helpers for schedule fire, chat/code runs, gateway message handling.
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::{bail, Context as _};
use pirs_agent::phase_agent::AgentPhaseDriver;
use pirs_agent::strategy::{run_strategy_async, PhaseReq, Task, ToolScope};
use pirs_agent::Agent;
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


pub fn load_all_skills(cwd: &Path, extra: Option<&Path>) -> Vec<Skill> {
    let mut skills = pirs_skills::discover_skills(cwd);
    if let Some(d) = extra {
        for sk in load_skills(d) {
            if !skills.iter().any(|s| s.name == sk.name) {
                skills.push(sk);
            }
        }
    }
    // Always include default home skills dir even if discover missed (empty home).
    for sk in load_skills(&default_skills_dir()) {
        if !skills.iter().any(|s| s.name == sk.name) {
            skills.push(sk);
        }
    }
    skills
}

/// Chat-safe tool set: recall + progressive skills + life tools (+ optional code tools).
pub fn chat_safe_tools(
    cwd: &Path,
    skills: &[Skill],
    allow_code: bool,
    allow_skill_manage: bool,
) -> Vec<Arc<dyn pirs_agent::AgentTool>> {
    chat_safe_tools_with_state(cwd, skills, allow_code, allow_skill_manage, None, None)
}

/// Gateway/chat tools. When `state_dir` is set, `peer_scope` must be the caller's
/// `SessionId::key()` so `session_search` cannot read other peers' transcripts.
pub fn chat_safe_tools_with_state(
    cwd: &Path,
    skills: &[Skill],
    allow_code: bool,
    allow_skill_manage: bool,
    state_dir: Option<&Path>,
    peer_scope: Option<&str>,
) -> Vec<Arc<dyn pirs_agent::AgentTool>> {
    let skills_arc = Arc::new(skills.to_vec());
    let mut tools: Vec<Arc<dyn pirs_agent::AgentTool>> =
        vec![Arc::new(pirs_tools::RecallTool::default())];
    tools.extend(skill_tools(skills_arc, allow_skill_manage));
    tools.extend(life_tools(false));
    // Browser + vision on chat/gateway (SSRF-safe / path-contained).
    tools.extend(pirs_tools::browser_tools(cwd.to_path_buf()));
    #[cfg(feature = "cdp")]
    tools.extend(pirs_tools::cdp_tools(cwd.to_path_buf()));
    tools.extend(pirs_tools::vision_tools(cwd.to_path_buf()));
    // Desktop computer-use only when explicitly enabled (dangerous).
    if matches!(
        std::env::var("PIRS_COMPUTER_USE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    ) {
        tools.extend(pirs_tools::computer_tools(cwd.to_path_buf()));
    }
    if let Some(state) = state_dir {
        // Gateway: require explicit peer key on the tool instance (not env).
        if let Some(peer) = peer_scope {
            tools.push(pirs_claw::session_search::gateway_session_search_tool(
                state.to_path_buf(),
                peer,
            ));
        } else {
            // Owner/CLI path with state_dir but no peer: global search is OK.
            tools.push(pirs_claw::session_search::session_search_tool(
                state.to_path_buf(),
            ));
        }
    }
    if allow_code {
        tools.extend(coding_tools(cwd));
    }
    // Dedupe (coding_tools already includes browser/vision via default_tools).
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    tools
}

pub fn which_bin(name: &str) -> Option<std::path::PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

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

/// Load optional Rhai packs for claw chat/code (not gateway unless flagged).
pub fn load_claw_extensions(cwd: &Path, enabled: bool) -> Option<Arc<pirs_rhai::ExtensionHost>> {
    if !enabled {
        return None;
    }
    pirs_rhai::register_core_host_apis();
    let mut host = pirs_rhai::ExtensionHost::new();
    if let Ok(p) = pirs_rhai::discover::resolve_pack_profile(None, cwd) {
        pirs_rhai::weak_packs::load_profile_packs(&mut host, p.packs.as_deref());
    } else {
        pirs_rhai::weak_packs::load_into(&mut host);
    }
    host.load_default_dirs(cwd);
    if !host.load_errors.is_empty() {
        for e in &host.load_errors {
            eprintln!("[pirs-claw extensions: {e}]");
        }
    }
    let host = Arc::new(host);
    let n = host.tools().len();
    if n > 0 || !host.load_errors.is_empty() {
        eprintln!(
            "[pirs-claw extensions: {} tool(s) from packs; host APIs project_profile/skills_index]",
            n
        );
    }
    Some(host)
}

/// Profile denials + optional extension packs + audit log (Opus review §2.4).
///
/// Gateway/chat peers previously had only the tool *list* as policy. This wires
/// the same profile gate + audit listener the `pirs` CLI uses. Interactive
/// approval prompts are not available on remote channels; use
/// `PIRS_AGENT_PROFILE=plan|accept-edits|auto-approve` (default: accept-edits
/// for interactive, plan for unattended).
pub fn install_claw_safety(
    mut agent: Agent,
    unattended: bool,
    host: Option<&Arc<pirs_rhai::ExtensionHost>>,
) -> Agent {
    let profile = if unattended {
        pirs_tools::SafetyProfile::parse(
            &std::env::var("PIRS_CLAW_UNATTENDED_PROFILE").unwrap_or_else(|_| "plan".into()),
        )
        .unwrap_or(pirs_tools::SafetyProfile::Plan)
    } else {
        pirs_tools::SafetyProfile::parse(
            &std::env::var("PIRS_AGENT_PROFILE")
                .or_else(|_| std::env::var("PIRS_CLAW_PROFILE"))
                .unwrap_or_else(|_| "accept-edits".into()),
        )
        .unwrap_or(pirs_tools::SafetyProfile::AcceptEdits)
    };
    std::env::set_var("PIRS_AGENT_PROFILE", profile.name());

    if let Some(host) = host {
        let mut tools = agent.tools.clone();
        tools.extend(host.tools());
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
        agent = agent.with_tools(tools);
    }

    // Profile denials first, then pack before_tool_call (first blocker wins).
    let profile_hook = pirs_tools::profile_hook(profile);
    let mut hooks = host.map(|h| h.hooks()).unwrap_or_default();
    let prev = hooks.before_tool_call.take();
    hooks.before_tool_call = Some(std::sync::Arc::new(move |id, name, args| {
        if let Some(r) = profile_hook(id, name, args) {
            return Some(r);
        }
        if let Some(ref p) = prev {
            return p(id, name, args);
        }
        None
    }));
    agent = agent.with_hooks(hooks);

    let audit = pirs_agent::AuditLog::default_open();
    if pirs_agent::audit_enabled() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            eprintln!("[pirs-claw audit: {}]", audit.path().display());
        });
    }
    agent.subscribe(pirs_agent::audit_listener(audit));
    agent
}

pub async fn fire_schedule_job(
    job: &pirs_claw::ScheduleEntry,
    state: &Path,
    default_model: &str,
    all_skills: &[Skill],
) -> anyhow::Result<bool> {
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

pub async fn handle_gateway_message(
    state: &Path,
    cwd: &Path,
    model: &str,
    inbound: &InboundMessage,
    skills: &[Skill],
    allow_code_tools: bool,
) -> anyhow::Result<GatewayReply> {
    // Gateway: skill writes off unless PIRS_SKILL_WRITE=1 explicitly.
    if std::env::var("PIRS_SKILL_WRITE").is_err() && std::env::var("PIRS_CLAW_SKILL_WRITE").is_err()
    {
        std::env::set_var("PIRS_SKILL_WRITE", "0");
    }
    let sid = SessionId::from_inbound(inbound);
    let store = SessionStore::open_for(state, sid.clone())?;
    store.append("user", &inbound.text)?;
    let mem = memory_bridge::open_memory(state).ok();
    if let Some(ref m) = mem {
        memory_bridge::scope_session(m, &sid.key());
        memory_bridge::remember_turn(m, "user", &inbound.text);
    }
    let (provider, key, _) = registry::resolve_llm(model, 2)?;
    require_llm_key(key.as_deref())?;
    let key_for_learn = key.clone();
    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let mut sys = claw_system_prompt();
    sys.push_str(&skills_prompt_section(skills));
    if allow_code_tools {
        sys.push_str(&pirs_tools::detect_profile(cwd).prompt_section());
    }
    if let Some(ref m) = mem {
        sys.push_str(&memory_bridge::recall_context(m, &inbound.text, 5));
    }
    let attach_log = pirs_claw::attach::AttachmentLog::new();
    let out_dir = state.join("outbound").join(sid.key().replace('/', "_"));
    // Scope session_search to this peer only (never global on multi-tenant gateway).
    let mut tools = chat_safe_tools_with_state(
        cwd,
        skills,
        allow_code_tools,
        false,
        Some(state),
        Some(sid.key().as_str()),
    );
    tools.push(Arc::new(pirs_claw::attach::AttachFileTool::new(
        out_dir.clone(),
        attach_log.clone_handle(),
    )));
    let mut agent = Agent::new(provider.clone(), model)
        .with_system_prompt(sys)
        .with_tools(tools)
        .with_completion(completion);
    agent = install_claw_safety(agent, pirs_claw::is_unattended(), None);
    if let Ok(mut msgs) = store.to_agent_messages() {
        if let Some(pirs_ai::Message::User(_)) = msgs.last() {
            msgs.pop();
        }
        agent.messages = msgs;
    }
    let new_msgs = agent.prompt(&inbound.text).await?;
    let reply = extract_assistant_reply(&new_msgs).ok_or_else(|| {
        anyhow::anyhow!(
            "empty assistant reply ({})",
            empty_assistant_diag(&new_msgs)
        )
    })?;
    store.append("assistant", &reply)?;
    if let Some(ref m) = mem {
        memory_bridge::remember_turn(m, "assistant", &reply);
    }
    if pirs_claw::learn::learn_enabled_gateway() {
        pirs_claw::learn::maybe_memory_nudge(
            provider.clone(),
            model,
            key_for_learn.clone(),
            state,
            &sid.key(),
            &inbound.text,
            &reply,
        )
        .await;
        // Improve skills that were viewed this turn (Hermes-style self-improve).
        let transcript = pirs_claw::learn::session_transcript(&inbound.text, &reply, "gateway");
        // Long Telegram threads can crystallize skills (same gate as chat).
        if transcript.chars().count() >= 800 {
            let _ = pirs_claw::learn::maybe_crystallize_skill(
                provider.clone(),
                model,
                key_for_learn.clone(),
                &transcript,
                800,
            )
            .await;
        }
        for sk in skills {
            if reply.contains(&sk.name) || inbound.text.to_ascii_lowercase().contains(&sk.name) {
                let md = format!(
                    "---\nname: {}\ndescription: {}\n---\n\n{}",
                    sk.name, sk.description, sk.body
                );
                let _ = pirs_claw::learn::maybe_improve_skill(
                    provider.clone(),
                    model,
                    key_for_learn.clone(),
                    &sk.name,
                    &md,
                    &transcript,
                    400,
                )
                .await;
            }
        }
    }

    // Collect attachments: explicit attach_file tool, write tool (code mode), fenced files.
    let mut attachments = attach_log.take();
    for p in pirs_claw::attach::paths_from_write_results(&new_msgs) {
        if !attachments.iter().any(|x| x == &p) {
            attachments.push(p);
        }
    }
    if attachments.is_empty() {
        // Fallback: materialize named fenced code blocks as files to send.
        for p in pirs_claw::attach::materialize_fenced_files(&reply, &out_dir) {
            attachments.push(p);
        }
    }

    Ok(GatewayReply {
        text: reply,
        attachments,
    })
}

pub async fn run_chat(
    state: &Path,
    model: &str,
    cwd: &Path,
    text: &str,
    skills: &[Skill],
    do_learn: bool,
    load_ext: bool,
) -> anyhow::Result<()> {
    let inbound = InboundMessage::cli(text);
    let (provider, key, _reg) = registry::resolve_llm(model, 2)?;
    require_llm_key(key.as_deref())?;
    let key_for_learn = key.clone();
    let host = load_claw_extensions(cwd, load_ext);

    let sid = SessionId::cli_local();
    let store = SessionStore::open_for(state, sid.clone())?;
    store.append("user", text)?;
    let mem = memory_bridge::open_memory(state).ok();
    if let Some(ref m) = mem {
        memory_bridge::scope_session(m, &sid.key());
        memory_bridge::remember_turn(m, "user", text);
    }

    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let mut sys = claw_system_prompt();
    sys.push_str(&skills_prompt_section(skills));
    sys.push_str(&pirs_tools::detect_profile(cwd).prompt_section());
    if let Some(ref m) = mem {
        sys.push_str(&memory_bridge::recall_context(m, text, 5));
    }
    // Cron/heartbeat set PIRS_CLAW_UNATTENDED=1 — never install unrestricted bash
    // unless the operator opts in with PIRS_CLAW_SCHEDULE_CODE=1.
    let mut tools = if pirs_claw::is_unattended() {
        eprintln!("[pirs-claw] unattended tool profile (no bash/write by default)");
        pirs_claw::unattended_tools(cwd)
    } else {
        let mut t = pirs_tools::default_tools(cwd.to_path_buf());
        t.extend(chat_safe_tools(cwd, skills, false, true));
        t
    };
    // Dedupe by name (default_tools may already include recall).
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    let mut agent = Agent::new(provider.clone(), model)
        .with_system_prompt(sys)
        .with_tools(tools)
        .with_completion(completion);
    agent = install_claw_safety(agent, pirs_claw::is_unattended(), host.as_ref());

    let prior = store.load()?;
    if prior.len() > 1 {
        let mut msgs = store.to_agent_messages()?;
        if let Some(pirs_ai::Message::User(_)) = msgs.last() {
            msgs.pop();
        }
        agent.messages = msgs;
    }

    let new_msgs = agent
        .prompt(text)
        .await
        .map_err(|e| anyhow::anyhow!("agent error (no assistant reply recorded): {e}"))?;
    let reply = extract_assistant_reply(&new_msgs).ok_or_else(|| {
        anyhow::anyhow!(
            "empty assistant reply (nothing recorded as assistant; {})",
            empty_assistant_diag(&new_msgs)
        )
    })?;
    store.append("assistant", &reply)?;
    if let Some(ref m) = mem {
        memory_bridge::remember_turn(m, "assistant", &reply);
    }
    if do_learn {
        pirs_claw::learn::maybe_memory_nudge(
            provider.clone(),
            model,
            key_for_learn.clone(),
            state,
            &sid.key(),
            text,
            &reply,
        )
        .await;
        let transcript = pirs_claw::learn::session_transcript(text, &reply, "");
        let crystallized = pirs_claw::learn::maybe_crystallize_skill(
            provider.clone(),
            model,
            key_for_learn.clone(),
            &transcript,
            800,
        )
        .await;
        if crystallized.is_none() {
            // Try improve any installed skill mentioned in the turn.
            for sk in skills {
                if text.to_ascii_lowercase().contains(&sk.name)
                    || reply.to_ascii_lowercase().contains(&sk.name)
                {
                    let md = format!(
                        "---\nname: {}\ndescription: {}\n---\n\n{}",
                        sk.name, sk.description, sk.body
                    );
                    let _ = pirs_claw::learn::maybe_improve_skill(
                        provider.clone(),
                        model,
                        key_for_learn.clone(),
                        &sk.name,
                        &md,
                        &transcript,
                        400,
                    )
                    .await;
                }
            }
        }
    }
    CliChannel.deliver(&OutboundReply::to(&inbound, reply))?;
    eprintln!(
        "[pirs-claw chat: session {} exec={}]",
        store.path().display(),
        describe_exec_backend()
    );
    Ok(())
}

#[allow(clippy::too_many_arguments)] // thin CLI wiring; not a public API surface
pub async fn run_code(
    cwd: &Path,
    model: &str,
    plan_model: &str,
    strategy_name: &str,
    prompt: &str,
    max_turns: Option<usize>,
    sequential: bool,
    skills: &[Skill],
    do_learn: bool,
    load_ext: bool,
) -> anyhow::Result<()> {
    let opts = apply_code_defaults(CodeOptions {
        cwd: cwd.to_path_buf(),
        model: model.into(),
        plan_model: if plan_model.is_empty() {
            None
        } else {
            Some(plan_model.into())
        },
        strategy: strategy_name.into(),
        prompt: Some(prompt.into()),
        max_turns,
        sequential,
    });

    let strategy = resolve_code_strategy(&opts)?;
    eprintln!(
        "[pirs-claw code: cwd={} model={} plan_model={:?} strategy={} phases={} exec={}]",
        opts.cwd.display(),
        opts.model,
        opts.plan_model,
        strategy.name,
        strategy.steps.len(),
        describe_exec_backend()
    );

    let retries = if sequential { 3 } else { 2 };
    let (provider, key, _) = registry::resolve_llm(&opts.model, retries)?;
    require_llm_key(key.as_deref())?;
    let host = load_claw_extensions(&opts.cwd, load_ext);
    let completion = pirs_ai::CompletionOptions {
        api_key: key,
        ..Default::default()
    };
    let skill_section = skills_prompt_section(skills);
    let project_section = pirs_tools::detect_profile(&opts.cwd).prompt_section();
    let key_for_learn = completion.api_key.clone();
    let skills_owned: Vec<Skill> = skills.to_vec();
    let host_c = host.clone();

    if strategy.name != "monolithic" && strategy.steps.len() > 1 {
        let opts_c = opts.clone();
        let provider_c = provider.clone();
        let completion_c = completion.clone();
        let skill_section_c = skill_section.clone();
        let project_section_c = project_section.clone();
        let skills_c = skills_owned.clone();
        let mut driver = AgentPhaseDriver::new(move |req: &PhaseReq| {
            let model = req.model.clone().unwrap_or_else(|| opts_c.model.clone());
            let mut tools = coding_tools(&opts_c.cwd);
            tools.extend(chat_safe_tools(&opts_c.cwd, &skills_c, false, true));
            {
                let mut seen = std::collections::HashSet::new();
                tools.retain(|t| seen.insert(t.name().to_string()));
            }
            if req.scope == ToolScope::ReadOnly {
                tools.retain(|t| {
                    matches!(
                        t.name(),
                        "read"
                            | "grep"
                            | "find"
                            | "ls"
                            | "code_map"
                            | "code_search"
                            | "recall"
                            | "skill_list"
                            | "skill_view"
                            | "web_fetch"
                            | "web_search"
                            | "project"
                            | "run_tests"
                    )
                });
            }
            let mut system = if req.system.trim().is_empty() {
                coding_system_prompt(&opts_c.cwd)
            } else {
                req.system.clone()
            };
            system.push_str(&skill_section_c);
            system.push_str(&project_section_c);
            let cwd_for_sub = opts_c.cwd.clone();
            let sub = pirs_agent::delegate::DelegateTool::new(
                provider_c.clone(),
                opts_c.model.clone(),
                completion_c.clone(),
                move || coding_tools(&cwd_for_sub),
            );
            tools.push(sub);
            let mut agent = Agent::new(provider_c.clone(), model)
                .with_system_prompt(system)
                .with_tools(tools)
                .with_completion(completion_c.clone());
            agent = install_claw_safety(agent, false, host_c.as_ref());
            if let Some(n) = opts_c.max_turns {
                agent.budgets.max_turns = Some(n);
            }
            if opts_c.sequential {
                agent = agent.with_tool_execution(pirs_agent::ExecutionMode::Sequential);
            }
            agent
        });

        let task = Task {
            issue: prompt.to_string(),
            targets: Vec::new(),
            verdict: None,
        };
        run_strategy_async(&strategy, &mut driver, &task).await?;
        let reply = extract_assistant_reply(driver.messages())
            .unwrap_or_else(|| "(strategy completed; no final assistant text)".into());
        if do_learn {
            let transcript = pirs_claw::learn::session_transcript(prompt, &reply, "code strategy run");
            let _ = pirs_claw::learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
        return Ok(());
    }

    let mut sys = coding_system_prompt(&opts.cwd);
    sys.push_str(&skill_section);
    sys.push_str(&project_section);
    let cwd_for_sub = opts.cwd.clone();
    let sub = pirs_agent::delegate::DelegateTool::new(
        provider.clone(),
        opts.model.clone(),
        completion.clone(),
        move || coding_tools(&cwd_for_sub),
    );
    let mut tools = coding_tools(&opts.cwd);
    tools.extend(chat_safe_tools(&opts.cwd, skills, false, true));
    {
        let mut seen = std::collections::HashSet::new();
        tools.retain(|t| seen.insert(t.name().to_string()));
    }
    tools.push(sub);
    let mut agent = build_code_agent(provider.clone(), &opts)
        .with_completion(completion)
        .with_system_prompt(sys)
        .with_tools(tools);
    agent = install_claw_safety(agent, false, host.as_ref());
    let msgs = agent.prompt(prompt).await?;
    if let Some(reply) = extract_assistant_reply(&msgs) {
        if do_learn {
            let transcript = pirs_claw::learn::session_transcript(prompt, &reply, "code run");
            let _ = pirs_claw::learn::maybe_crystallize_skill(
                provider,
                model,
                key_for_learn,
                &transcript,
                400,
            )
            .await;
        }
        println!("{reply}");
    } else {
        anyhow::bail!(
            "empty assistant reply ({})",
            empty_assistant_diag(&msgs)
        );
    }
    Ok(())
}
