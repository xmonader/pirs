//! Coding presets (plan-exec defaults, tools, strategy pin).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use pirs_agent::strategy::{pin_plan_model, Strategy, ToolScope};
use pirs_agent::{Agent, AgentTool};
use pirs_ai::LlmProvider;

/// Default strategy for coding (plan then execute).
pub const DEFAULT_STRATEGY: &str = "plan-exec";

/// Default exec model alias (cheap).
pub const DEFAULT_MODEL: &str = "qwen3.5-plus";

/// Default planner model alias (strong).
pub const DEFAULT_PLAN_MODEL: &str = "deepseek-v4-pro";

/// Options for a coding run.
#[derive(Debug, Clone)]
pub struct CodeOptions {
    pub cwd: PathBuf,
    pub model: String,
    pub plan_model: Option<String>,
    pub strategy: String,
    pub prompt: Option<String>,
    pub max_turns: Option<usize>,
    pub sequential: bool,
}

impl Default for CodeOptions {
    fn default() -> Self {
        CodeOptions {
            cwd: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            model: DEFAULT_MODEL.into(),
            plan_model: Some(DEFAULT_PLAN_MODEL.into()),
            strategy: DEFAULT_STRATEGY.into(),
            prompt: None,
            max_turns: Some(40),
            sequential: false,
        }
    }
}

/// Apply coding defaults without clobbering explicit non-empty fields.
pub fn apply_code_defaults(mut opts: CodeOptions) -> CodeOptions {
    if opts.model.is_empty() {
        opts.model = DEFAULT_MODEL.into();
    }
    if opts.strategy.is_empty() {
        opts.strategy = DEFAULT_STRATEGY.into();
    }
    if opts.plan_model.as_ref().is_some_and(|s| s.is_empty()) {
        opts.plan_model = Some(DEFAULT_PLAN_MODEL.into());
    }
    if opts.plan_model.is_none() {
        opts.plan_model = Some(DEFAULT_PLAN_MODEL.into());
    }
    opts
}

/// Core coding tools for a workspace.
pub fn coding_tools(cwd: &Path) -> Vec<Arc<dyn AgentTool>> {
    pirs_tools::default_tools(cwd.to_path_buf())
}

/// Env set by cron/heartbeat/schedule tick so `chat` does not install bash.
pub const UNATTENDED_ENV: &str = "PIRS_CLAW_UNATTENDED";

/// True when this process is a scheduled/heartbeat unattended turn.
pub fn is_unattended() -> bool {
    matches!(
        std::env::var(UNATTENDED_ENV).as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    )
}

/// Restricted tool set for cron / heartbeat (no bash, write, edit by default).
///
/// Opt into full coding tools for scheduled jobs with `PIRS_CLAW_SCHEDULE_CODE=1`.
pub fn unattended_tools(cwd: &Path) -> Vec<Arc<dyn AgentTool>> {
    if matches!(
        std::env::var("PIRS_CLAW_SCHEDULE_CODE").as_deref(),
        Ok("1") | Ok("true") | Ok("yes") | Ok("on")
    ) {
        return coding_tools(cwd);
    }
    let mut tools: Vec<Arc<dyn AgentTool>> = vec![
        Arc::new(pirs_tools::ReadTool::new(cwd.to_path_buf())),
        Arc::new(pirs_tools::GrepTool::new(cwd.to_path_buf())),
        Arc::new(pirs_tools::FindTool::new(cwd.to_path_buf())),
        Arc::new(pirs_tools::LsTool::new(cwd.to_path_buf())),
        Arc::new(pirs_tools::RecallTool::default()),
        Arc::new(pirs_tools::ProjectTool::new(cwd.to_path_buf())),
    ];
    tools.extend(pirs_tools::life_tools(false));
    tools.extend(pirs_tools::browser_tools(cwd.to_path_buf()));
    // Explicitly no bash / write / edit / jobs / computer.
    let mut seen = std::collections::HashSet::new();
    tools.retain(|t| seen.insert(t.name().to_string()));
    tools
}

/// Tool names that must never appear in the default unattended profile.
pub fn unattended_forbidden_tool_names() -> &'static [&'static str] {
    &["bash", "write", "edit", "edit_block", "ast_edit"]
}

/// Optional safety profile from `PIRS_AGENT_PROFILE` (shared with pirs harness).
pub fn env_safety_profile() -> pirs_tools::SafetyProfile {
    std::env::var("PIRS_AGENT_PROFILE")
        .ok()
        .and_then(|s| pirs_tools::SafetyProfile::parse(&s))
        .unwrap_or(pirs_tools::SafetyProfile::Default)
}

/// Build an agent for coding (monolithic path / phase factory base).
pub fn build_code_agent(provider: Arc<dyn LlmProvider>, opts: &CodeOptions) -> Agent {
    let tools = coding_tools(&opts.cwd);
    let mut agent = Agent::new(provider, opts.model.clone())
        .with_system_prompt(coding_system_prompt(&opts.cwd))
        .with_tools(tools);
    let profile = env_safety_profile();
    // Normalize env so Rhai packs (strict-plan) see the same profile as the gate.
    std::env::set_var("PIRS_AGENT_PROFILE", profile.name());
    {
        let mut hooks = pirs_agent::Hooks::default();
        hooks.before_tool_call = Some(pirs_tools::profile_hook(profile));
        agent = agent.with_hooks(hooks);
    }
    // Always-on audit for code path (same as chat/gateway).
    let audit = pirs_agent::AuditLog::default_open();
    agent.subscribe(pirs_agent::audit_listener(audit));
    if let Some(n) = opts.max_turns {
        agent.budgets.max_turns = Some(n);
    }
    if opts.sequential {
        agent = agent.with_tool_execution(pirs_agent::ExecutionMode::Sequential);
    }
    agent
}

/// Resolve built-in strategy and pin plan model onto read-only phases.
pub fn resolve_code_strategy(opts: &CodeOptions) -> anyhow::Result<Strategy> {
    let mut s = pirs_rhai::builtins::builtin(&opts.strategy)
        .or_else(|| pirs_rhai::discover::resolve_strategy(&opts.strategy, &opts.cwd).ok())
        .ok_or_else(|| anyhow::anyhow!("unknown strategy {:?}", opts.strategy))?;
    if let Some(pm) = &opts.plan_model {
        pin_plan_model(&mut s, pm);
    }
    Ok(s)
}

/// Count read-only vs full phases after plan-model pin.
pub fn phase_scope_summary(strategy: &Strategy) -> (usize, usize, Vec<Option<String>>) {
    let mut ro = 0usize;
    let mut full = 0usize;
    let mut models = Vec::new();
    for step in &strategy.steps {
        match step {
            pirs_agent::strategy::Step::Solo(p) => {
                match p.scope {
                    ToolScope::ReadOnly => ro += 1,
                    ToolScope::Full => full += 1,
                }
                models.push(p.model.clone());
            }
            pirs_agent::strategy::Step::Fan { branches, .. } => {
                for p in branches {
                    match p.scope {
                        ToolScope::ReadOnly => ro += 1,
                        ToolScope::Full => full += 1,
                    }
                    models.push(p.model.clone());
                }
            }
        }
    }
    (ro, full, models)
}

pub fn coding_system_prompt(cwd: &Path) -> String {
    format!(
        "You are pirs-claw in coding mode, working in `{}`.\n\
         Prefer small, correct edits. Use tools to read and search before writing.\n\
         Fix source, not tests, unless the user asked to change tests.\n\
         Run project tests after edits when possible. Be concise.",
        cwd.display()
    )
}

/// Whether cwd looks like a software project (prefer code profile).
pub fn looks_like_repo(cwd: &Path) -> bool {
    cwd.join(".git").exists()
        || cwd.join("Cargo.toml").exists()
        || cwd.join("package.json").exists()
        || cwd.join("pyproject.toml").exists()
        || cwd.join("go.mod").exists()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_agent::strategy::ToolScope;

    #[test]
    fn defaults_are_coding_oriented() {
        let mut o = CodeOptions::default();
        o.model.clear();
        o.strategy.clear();
        o.plan_model = None;
        let o = apply_code_defaults(o);
        assert_eq!(o.model, DEFAULT_MODEL);
        assert_eq!(o.strategy, DEFAULT_STRATEGY);
        assert_eq!(o.plan_model.as_deref(), Some(DEFAULT_PLAN_MODEL));
    }

    #[test]
    fn coding_tools_include_core_names() {
        let dir = tempfile::tempdir().unwrap();
        let tools = coding_tools(dir.path());
        let names: Vec<_> = tools.iter().map(|t| t.name().to_string()).collect();
        for need in ["read", "write", "edit", "bash", "grep", "find", "ls"] {
            assert!(
                names.iter().any(|n| n == need),
                "missing tool {need} in {names:?}"
            );
        }
    }

    #[test]
    fn unattended_tools_exclude_bash_by_default() {
        let dir = tempfile::tempdir().unwrap();
        // Ensure schedule-code opt-in is off for this process.
        std::env::remove_var("PIRS_CLAW_SCHEDULE_CODE");
        let tools = unattended_tools(dir.path());
        let names: Vec<_> = tools.iter().map(|t| t.name().to_string()).collect();
        for bad in unattended_forbidden_tool_names() {
            assert!(
                !names.iter().any(|n| n == *bad),
                "unattended must not include {bad}: {names:?}"
            );
        }
        assert!(names.iter().any(|n| n == "read"));
        assert!(names.iter().any(|n| n == "web_fetch") || names.iter().any(|n| n == "web_search"));
    }

    #[test]
    fn plan_exec_strategy_pins_plan_model_on_readonly_only() {
        let opts = CodeOptions {
            strategy: "plan-exec".into(),
            plan_model: Some("deepseek-v4-pro".into()),
            model: "qwen3.5-plus".into(),
            ..CodeOptions::default()
        };
        let s = resolve_code_strategy(&opts).expect("plan-exec builtin");
        assert_eq!(s.name, "plan-exec");
        let (ro, full, models) = phase_scope_summary(&s);
        assert!(ro >= 1, "need plan phase");
        assert!(full >= 1, "need exec phase");
        match &s.steps[0] {
            pirs_agent::strategy::Step::Solo(p) => {
                assert_eq!(p.scope, ToolScope::ReadOnly);
                assert_eq!(p.model.as_deref(), Some("deepseek-v4-pro"));
            }
            _ => panic!("expected solo plan"),
        }
        match s.steps.last() {
            Some(pirs_agent::strategy::Step::Solo(p)) => {
                assert_eq!(p.scope, ToolScope::Full);
                assert!(p.model.is_none(), "exec keeps run default model");
            }
            _ => panic!("expected solo exec"),
        }
        assert!(models
            .iter()
            .any(|m| m.as_deref() == Some("deepseek-v4-pro")));
    }

    #[test]
    fn looks_like_repo_detects_cargo() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!looks_like_repo(dir.path()));
        std::fs::write(dir.path().join("Cargo.toml"), "[package]\nname=\"x\"\n").unwrap();
        assert!(looks_like_repo(dir.path()));
    }
}
