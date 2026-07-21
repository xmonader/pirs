//! Shared skills + learning loop for **pirs** and **pirs-claw**.
//!
//! - agentskills.io layout and validation
//! - progressive disclosure (name/description in prompt; body via skill_view)
//! - multi-root discovery (project + home, `.pirs` / `.agents` / `.claude`)
//! - optional post-turn memory nudge + skill crystallize

pub mod learn;
pub mod skill;
pub mod tools;

pub use learn::{
    learn_enabled_cli, learn_enabled_gateway, learn_enabled_interactive, maybe_crystallize_skill,
    maybe_memory_nudge, session_transcript, LEARN_DISABLE_ENV, LEARN_GATEWAY_ENV,
};
pub use skill::{
    default_skills_dir, discover_skills, find_skill, install_skill, install_skill_url, load_skills,
    parse_skill_md, read_skill_resource, record_usage, remove_skill, select_skills, skill_roots,
    skills_full_section, skills_prompt_section, usage_counts, validate_description,
    validate_skill, validate_skill_name, write_skill, Skill,
};
pub use tools::{skill_tools, skill_write_allowed, SKILL_WRITE_ENV};
