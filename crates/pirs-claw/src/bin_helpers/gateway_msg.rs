//! gateway_msg.rs
use std::path::Path;
use std::sync::Arc;

use pirs_agent::Agent;
use pirs_claw::channel::InboundMessage;
use pirs_claw::memory_bridge;
use pirs_claw::registry;
use pirs_claw::{
    empty_assistant_diag, extract_assistant_reply, require_llm_key, GatewayReply, SessionId,
    SessionStore,
};
use pirs_skills::{skills_prompt_section, Skill};

use super::tools::{chat_safe_tools_with_state, install_claw_safety};

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
    // Session-stable system (soul frozen + memory digest). Turn recall → user msg.
    let mem_digest = mem
        .as_ref()
        .map(|m| memory_bridge::session_memory_digest(m, 5))
        .unwrap_or_default();
    let mut sys = pirs_claw::claw_system_prompt_with_memory(&mem_digest);
    sys.push_str(&skills_prompt_section(skills));
    if allow_code_tools {
        sys.push_str(&pirs_tools::detect_profile(cwd).prompt_section());
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
    let prompt_text = memory_bridge::user_text_with_turn_recall(mem.as_deref(), &inbound.text, 5);
    let new_msgs = agent.prompt(prompt_text).await?;
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
        // Crystallize / improve in background — don't block Telegram reply.
        let transcript = pirs_claw::learn::session_transcript(&inbound.text, &reply, "gateway");
        let skills_owned: Vec<(String, String, String)> = skills
            .iter()
            .map(|sk| (sk.name.clone(), sk.description.clone(), sk.body.clone()))
            .collect();
        let inbound_l = inbound.text.to_ascii_lowercase();
        let reply_c = reply.clone();
        let provider_bg = provider.clone();
        let model_bg = model.to_string();
        let key_bg = key_for_learn.clone();
        tokio::spawn(async move {
            if transcript.chars().count() >= 800 {
                let _ = pirs_claw::learn::maybe_crystallize_skill(
                    provider_bg.clone(),
                    &model_bg,
                    key_bg.clone(),
                    &transcript,
                    800,
                )
                .await;
            }
            for (name, description, body) in skills_owned {
                if reply_c.contains(&name) || inbound_l.contains(&name) {
                    let md =
                        format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}");
                    let _ = pirs_claw::learn::maybe_improve_skill(
                        provider_bg.clone(),
                        &model_bg,
                        key_bg.clone(),
                        &name,
                        &md,
                        &transcript,
                        400,
                    )
                    .await;
                }
            }
        });
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
