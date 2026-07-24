//! Shared safety floor for headless modes (rpc / acp / serve).
//!
//! Interactive `pirs` installs approval + profile + live permission + audit
//! after a large wiring block. `--mode rpc` / `--mode acp` historically returned
//! early and re-read only raw env vars (ignoring resolved CLI flags). This
//! module is the single place those modes call so they cannot diverge.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use pirs_agent::events::{AfterToolCallHook, BeforeToolCallHook};
use pirs_agent::{Agent, Hooks};
use pirs_tools::{PermissionMode, SafetyProfile};

use crate::approval::{ApprovalGate, ApprovalMode};

/// Resolved safety settings passed from CLI (not re-read from env alone).
#[derive(Debug, Clone)]
pub struct SafetyConfig {
    pub approval: ApprovalMode,
    pub profile: SafetyProfile,
    pub permission: PermissionMode,
    pub cwd: PathBuf,
    /// LLM provider name as resolved by main (`openai` | `anthropic`).
    pub provider: String,
}

impl SafetyConfig {
    /// Build from already-resolved CLI strings (after config_file layering).
    pub fn from_resolved(
        cwd: PathBuf,
        approval: &str,
        agent_profile: &str,
        permission_mode: Option<&str>,
        provider: &str,
    ) -> Self {
        let approval = ApprovalMode::parse(approval).unwrap_or(ApprovalMode::Auto);
        let profile = SafetyProfile::parse(agent_profile).unwrap_or(SafetyProfile::Default);
        let permission = permission_mode
            .and_then(PermissionMode::parse)
            .unwrap_or_else(PermissionMode::from_env);
        Self {
            approval,
            profile,
            permission,
            cwd,
            provider: provider.to_string(),
        }
    }

    /// Env-only fallback for tests / direct library use of rpc without main.
    #[cfg(test)]
    pub fn from_env(cwd: PathBuf) -> Self {
        let approval = std::env::var("PIRS_APPROVAL")
            .ok()
            .and_then(|m| ApprovalMode::parse(&m))
            .unwrap_or(ApprovalMode::Auto);
        let profile = std::env::var("PIRS_AGENT_PROFILE")
            .ok()
            .and_then(|s| SafetyProfile::parse(&s))
            .unwrap_or(SafetyProfile::Default);
        let permission = std::env::var("PIRS_PERMISSION_MODE")
            .ok()
            .and_then(|s| PermissionMode::parse(&s))
            .unwrap_or_else(PermissionMode::from_env);
        let provider = std::env::var("PIRS_PROVIDER").unwrap_or_else(|_| "openai".into());
        Self {
            approval,
            profile,
            permission,
            cwd,
            provider,
        }
    }
}

/// Whether a provider name selects Anthropic (vs OpenAI-compat).
pub fn provider_is_anthropic(provider: &str) -> bool {
    provider.eq_ignore_ascii_case("anthropic")
}

/// Build the composed `before_tool_call` gate used by headless modes.
///
/// Order: approval/profile gate → live permission ladder → `extra_before`
/// (ACP client permission, pack hooks, …).
///
/// Returns the hook plus the `ApprovalGate` Arc that must stay alive for the
/// session (Ask mode remembers "always" on that gate).
pub fn compose_safety_before_hook(
    cfg: &SafetyConfig,
    extra_before: Option<BeforeToolCallHook>,
) -> (BeforeToolCallHook, Arc<ApprovalGate>) {
    pirs_tools::init_live_permission_mode(cfg.permission);
    std::env::set_var("PIRS_AGENT_PROFILE", cfg.profile.name());

    let gate = Arc::new(ApprovalGate::with_profile(
        cfg.approval,
        cfg.cwd.clone(),
        cfg.profile,
    ));
    let mut gate_hook = if cfg.approval == ApprovalMode::Ask
        || cfg.profile != SafetyProfile::Default
    {
        Some(gate.hook())
    } else {
        None
    };
    // Always install live permission ladder (plan/act mid-session).
    gate_hook = Hooks::chain_before(gate_hook, Some(pirs_tools::live_permission_hook()));
    gate_hook = Hooks::chain_before(gate_hook, extra_before);
    let hook = gate_hook.expect("live permission ladder always yields a before_tool hook");
    (hook, gate)
}

/// Install profile denials, optional Ask approval, live permission ladder, and
/// audit log. Chains `extra_before` after the safety gate (e.g. ACP client
/// permission prompts, extension pack hooks).
///
/// Returns the agent with hooks + audit subscriber applied.
pub fn install_safety_floor(
    mut agent: Agent,
    cfg: &SafetyConfig,
    extra_before: Option<BeforeToolCallHook>,
    mut extra_hooks: Hooks,
) -> Agent {
    let pack_before = extra_hooks.before_tool_call.take();
    let chained_extra = Hooks::chain_before(extra_before, pack_before);
    let (hook, gate) = compose_safety_before_hook(cfg, chained_extra);
    extra_hooks.before_tool_call = Some(hook);
    agent = agent.with_hooks(extra_hooks);

    // First-class audit (disable with PIRS_AUDIT=0).
    let audit = pirs_agent::AuditLog::default_open();
    agent.subscribe(pirs_agent::audit_listener(audit));

    // Keep gate alive for the session (Ask mode remembers "always").
    std::mem::forget(gate);
    agent
}

/// Fill the sub-agent policy slot so sub-agents inherit the same safety floor
/// as the parent — even when no extension packs provide before/after hooks
/// (mirrors interactive `main.rs` fallback when `policy_slot` is empty).
pub fn fill_subagent_policy_slot(
    slot: &Mutex<Option<(BeforeToolCallHook, AfterToolCallHook)>>,
    cfg: &SafetyConfig,
    ext_before: Option<BeforeToolCallHook>,
    ext_after: Option<AfterToolCallHook>,
) {
    let (hook, gate) = compose_safety_before_hook(cfg, ext_before);
    let after: AfterToolCallHook = ext_after.unwrap_or_else(|| {
        Arc::new(|_id, _name, _result| None)
    });
    *slot.lock().unwrap() = Some((hook, after));
    std::mem::forget(gate);
}

/// Pure: which tools are blocked under plan for process spawning.
/// Production denials go through the installed before-tool hook; this helper is
/// the same predicate for unit tests without installing hooks.
#[cfg(test)]
pub fn plan_blocks_process_tool(tool: &str, args: &serde_json::Value) -> bool {
    pirs_tools::profile_deny_reason_with_args(SafetyProfile::Plan, tool, args).is_some()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn provider_anthropic_detection() {
        assert!(provider_is_anthropic("anthropic"));
        assert!(provider_is_anthropic("Anthropic"));
        assert!(!provider_is_anthropic("openai"));
    }

    #[test]
    fn from_resolved_prefers_cli_strings_not_env() {
        std::env::set_var("PIRS_PROVIDER", "anthropic");
        let cfg = SafetyConfig::from_resolved(
            PathBuf::from("/work"),
            "ask",
            "plan",
            Some("read-only"),
            "openai",
        );
        assert_eq!(cfg.provider, "openai");
        assert_eq!(cfg.approval, ApprovalMode::Ask);
        assert_eq!(cfg.profile, SafetyProfile::Plan);
        std::env::remove_var("PIRS_PROVIDER");
    }

    #[test]
    fn from_env_reads_process_env() {
        std::env::set_var("PIRS_PROVIDER", "anthropic");
        std::env::set_var("PIRS_APPROVAL", "yolo");
        std::env::set_var("PIRS_AGENT_PROFILE", "plan");
        let cfg = SafetyConfig::from_env(PathBuf::from("/tmp/work"));
        assert_eq!(cfg.provider, "anthropic");
        assert_eq!(cfg.approval, ApprovalMode::Yolo);
        assert_eq!(cfg.profile, SafetyProfile::Plan);
        assert_eq!(cfg.cwd, PathBuf::from("/tmp/work"));
        std::env::remove_var("PIRS_PROVIDER");
        std::env::remove_var("PIRS_APPROVAL");
        std::env::remove_var("PIRS_AGENT_PROFILE");
    }

    #[test]
    fn plan_blocks_bash_and_project_test_not_list() {
        assert!(plan_blocks_process_tool("bash", &json!({})));
        assert!(plan_blocks_process_tool(
            "project",
            &json!({"action": "test"})
        ));
        assert!(!plan_blocks_process_tool(
            "project",
            &json!({"action": "list"})
        ));
        assert!(!plan_blocks_process_tool("read", &json!({})));
        assert!(plan_blocks_process_tool("run_tests", &json!({})));
    }

    #[test]
    fn compose_safety_before_hook_denies_bash_under_plan() {
        // Drives the *real* installed before_tool hook, not a re-implementation.
        let cfg = SafetyConfig {
            approval: ApprovalMode::Auto,
            profile: SafetyProfile::Plan,
            permission: PermissionMode::ReadOnly,
            cwd: PathBuf::from("/tmp"),
            provider: "openai".into(),
        };
        let (hook, _gate) = compose_safety_before_hook(&cfg, None);
        let deny = hook("call-1", "bash", &json!({"command": "echo hi"}));
        assert!(
            deny.is_some(),
            "plan profile hook must deny bash; got None (install would be a no-op)"
        );
        assert!(
            deny.as_deref().unwrap().contains("plan")
                || deny.as_deref().unwrap().contains("bash")
                || deny.as_deref().unwrap().contains("read-only"),
            "unexpected deny text: {deny:?}"
        );
        // list-only project must still be allowed under plan.
        let allow = hook("call-2", "project", &json!({"action": "list"}));
        assert!(
            allow.is_none(),
            "project list must not be denied under plan: {allow:?}"
        );
        let deny_test = hook("call-3", "project", &json!({"action": "test"}));
        assert!(
            deny_test.is_some(),
            "project test must be denied under plan"
        );
    }

    #[test]
    fn fill_subagent_policy_slot_always_populated_without_packs() {
        let cfg = SafetyConfig {
            approval: ApprovalMode::Auto,
            profile: SafetyProfile::Plan,
            permission: PermissionMode::ReadOnly,
            cwd: PathBuf::from("/tmp"),
            provider: "openai".into(),
        };
        let slot: Mutex<Option<(BeforeToolCallHook, AfterToolCallHook)>> =
            Mutex::new(None);
        fill_subagent_policy_slot(&slot, &cfg, None, None);
        let guard = slot.lock().unwrap();
        let (before, _after) = guard.as_ref().expect("slot must be filled without packs");
        let deny = before("id", "bash", &json!({"command": "true"}));
        assert!(
            deny.is_some(),
            "sub-agent policy without packs must still deny bash under plan"
        );
    }

    /// Regression: production rpc_mode/acp_mode must call shared assembly.
    /// Asserts on the production source slice only (before any `#[cfg(test)]`).
    #[test]
    fn rpc_and_acp_source_invoke_shared_safety_floor() {
        let rpc = include_str!("rpc_mode.rs");
        let acp = include_str!("acp_mode.rs");
        let rpc_prod = rpc.split("#[cfg(test)]").next().unwrap_or(rpc);
        let acp_prod = acp.split("#[cfg(test)]").next().unwrap_or(acp);
        for (name, prod) in [("rpc_mode", rpc_prod), ("acp_mode", acp_prod)] {
            assert!(
                prod.contains("install_safety_floor"),
                "{name} production code must call install_safety_floor"
            );
            assert!(
                prod.contains("SafetyConfig::from_resolved"),
                "{name} production code must use SafetyConfig::from_resolved"
            );
            assert!(
                prod.contains("fill_subagent_policy_slot"),
                "{name} production code must fill subagent policy without requiring packs"
            );
            assert!(
                prod.contains("provider_is_anthropic"),
                "{name} production code must select provider from resolved CLI, not env alone"
            );
            assert!(
                prod.contains("safety_cfg.provider"),
                "{name} must consume SafetyConfig.provider (not leave the pin unread)"
            );
            // Must not re-fork opts.provider for anthropic detection after snapshot resolve.
            assert!(
                !prod.contains("provider_is_anthropic(&opts.provider)"),
                "{name} must not bypass safety_cfg.provider for anthropic detection"
            );
        }
    }
}
