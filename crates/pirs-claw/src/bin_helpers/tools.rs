//! tools.rs
use std::path::Path;
use std::sync::Arc;

use pirs_agent::Agent;
use pirs_claw::presets::coding_tools;
use pirs_skills::{
    default_skills_dir, load_skills,
    skill_tools, Skill,
};
use pirs_tools::life_tools;


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

