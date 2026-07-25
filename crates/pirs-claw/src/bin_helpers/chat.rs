//! chat.rs
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

use super::tools::{
    chat_safe_tools, install_claw_safety, load_claw_extensions,
};

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

