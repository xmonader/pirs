//! gateway_msg.rs
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
    chat_safe_tools_with_state, install_claw_safety, load_claw_extensions,
};

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

