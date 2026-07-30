//! Shared skills + learning loop for **pirs** and **pirs-claw**.
//!
//! - agentskills.io layout and validation
//! - progressive disclosure (name/description in prompt; body via skill_view)
//! - multi-root discovery (project + home, `.pirs` / `.agents` / `.claude`)
//! - optional post-turn memory nudge + skill crystallize

pub mod heartbeat;
pub mod learn;
pub mod skill;
pub mod soul;
pub mod tools;

pub use heartbeat::{
    due as heartbeat_due, ensure_template as ensure_heartbeat_template,
    maybe_prompt as heartbeat_prompt, DEFAULT_MIN_INTERVAL_SECS,
};
pub use learn::{
    evolution_write_dir, learn_enabled_cli, learn_enabled_gateway, learn_enabled_interactive,
    looks_durable, maybe_crystallize_skill, maybe_improve_skill, maybe_memory_nudge,
    maybe_update_soul, record_evolution_case, session_transcript, EvolutionMode, LEARN_DISABLE_ENV,
    LEARN_GATEWAY_ENV,
};
pub use skill::{
    default_skills_dir, discover_skills, ensure_bundled_skills, find_skill, install_skill,
    install_skill_url, load_skills, parse_skill_md, read_skill_resource, record_usage,
    remove_skill, select_skills, skill_roots, skills_full_section, skills_prompt_section,
    usage_counts, validate_description, validate_skill, validate_skill_name, write_skill, Skill,
};
pub use soul::{
    curator_report, default_soul_path, default_soul_template, invalidate_session_identity,
    merge_soul_updates, read_soul, session_identity, session_soul_prompt_section,
    set_session_identity, soul_prompt_section, write_soul, SessionIdentitySnapshot,
};
pub use tools::{skill_tools, skill_write_allowed, SKILL_WRITE_ENV};
