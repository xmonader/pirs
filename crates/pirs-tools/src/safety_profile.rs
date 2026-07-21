//! Named safety profiles (Vibe-class: plan / accept-edits / auto-approve).
//!
//! Profiles are enforced at the `before_tool_call` gate as hard deny/allow
//! rules. They compose with approval modes (`auto`/`ask`/`yolo`):
//! - `plan` — only non-mutating tools
//! - `accept_edits` — file mutations allowed; shell still subject to approval
//! - `auto_approve` — all tools allowed (no profile denials)
//!
//! **Rhai packs may only ADD denials** (e.g. `extensions/strict-plan.rhai`).
//! They must never loosen this module’s hard denials. Host query
//! `agent_profile("")` exposes the active profile name to packs.

use pirs_agent::events::BeforeToolCallHook;
use std::sync::Arc;

/// Named agent safety profile (Vibe equivalents).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SafetyProfile {
    /// Default: no extra profile denials (approval mode still applies).
    Default,
    /// Read-only exploration / planning (Vibe `plan`).
    Plan,
    /// Auto-allow file edits; bash still gated by approval (Vibe `accept-edits`).
    AcceptEdits,
    /// Auto-allow all tools (Vibe `auto-approve` / yolo-class).
    AutoApprove,
}

impl SafetyProfile {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "" | "default" => Some(Self::Default),
            "plan" => Some(Self::Plan),
            "accept-edits" | "accept_edits" | "acceptedits" => Some(Self::AcceptEdits),
            "auto-approve" | "auto_approve" | "autoapprove" | "yolo" => Some(Self::AutoApprove),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Plan => "plan",
            Self::AcceptEdits => "accept-edits",
            Self::AutoApprove => "auto-approve",
        }
    }
}

/// Tools that only observe state (safe under `plan`).
pub fn is_readonly_tool(tool: &str) -> bool {
    matches!(
        tool,
        "read"
            | "grep"
            | "find"
            | "ls"
            | "recall"
            | "web_fetch"
            | "web_search"
            | "project"
            | "ask_user"
            | "todo"
            | "vision_describe"
            | "browser_navigate"
            | "browser_screenshot"
            | "skill_list"
            | "skill_view"
            | "session_search"
            | "jobs"
            | "job_output"
            | "job_wait"
            | "wait_ready"
            | "use_tool"
            | "code_search"
            | "code_map"
            | "semantic_search"
            | "lsp"
            | "pr"
            | "doctor"
            | "audit_tail"
            | "research"
            | "session_rewind"
            | "fleet"
            | "checkpoint"
            | "git"
    )
}

/// File mutation tools (auto-allowed under `accept-edits`).
pub fn is_file_mutation_tool(tool: &str) -> bool {
    matches!(
        tool,
        "write" | "edit" | "edit_block" | "safe_edit" | "ast_edit"
    )
}

/// Shell / process tools (still approval-gated under `accept-edits`).
pub fn is_shell_tool(tool: &str) -> bool {
    matches!(
        tool,
        "bash" | "run_tests" | "job_kill" | "job_steer" | "computer_click" | "computer_type"
    ) || tool.starts_with("computer_")
}

/// Whether the profile hard-denies this tool. `None` = allowed by profile.
pub fn profile_deny_reason(profile: SafetyProfile, tool: &str) -> Option<String> {
    match profile {
        SafetyProfile::Default | SafetyProfile::AutoApprove => None,
        SafetyProfile::Plan => {
            if is_readonly_tool(tool) {
                None
            } else {
                Some(format!(
                    "tool `{tool}` blocked by safety profile `plan` (read-only; \
                     use profile accept-edits or auto-approve to mutate)"
                ))
            }
        }
        SafetyProfile::AcceptEdits => {
            // File tools + readonly always ok; shell ok (approval may still ask);
            // block only explicitly destructive computer control if desired —
            // Vibe accept-edits allows bash with normal permissions.
            // We allow all non-plan-blocked tools here; shell is not profile-denied.
            let _ = is_shell_tool(tool);
            let _ = is_file_mutation_tool(tool);
            None
        }
    }
}

/// Whether approval should auto-pass (never prompt) for this tool under the profile.
///
/// - `auto_approve`: always skip approval prompts
/// - `accept_edits`: skip prompts for file mutations and readonly tools
/// - `plan` / `default`: leave approval mode unchanged
pub fn profile_skips_approval(profile: SafetyProfile, tool: &str) -> bool {
    match profile {
        SafetyProfile::AutoApprove => true,
        SafetyProfile::AcceptEdits => is_file_mutation_tool(tool) || is_readonly_tool(tool),
        SafetyProfile::Plan | SafetyProfile::Default => false,
    }
}

/// Build a `before_tool_call` hook that enforces profile denials.
pub fn profile_hook(profile: SafetyProfile) -> BeforeToolCallHook {
    Arc::new(move |_id, tool, _args| profile_deny_reason(profile, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_names() {
        assert_eq!(SafetyProfile::parse("plan"), Some(SafetyProfile::Plan));
        assert_eq!(
            SafetyProfile::parse("accept-edits"),
            Some(SafetyProfile::AcceptEdits)
        );
        assert_eq!(
            SafetyProfile::parse("auto-approve"),
            Some(SafetyProfile::AutoApprove)
        );
        assert_eq!(
            SafetyProfile::parse("yolo"),
            Some(SafetyProfile::AutoApprove)
        );
        assert!(SafetyProfile::parse("nope").is_none());
    }

    #[test]
    fn plan_blocks_bash_and_write_allows_read() {
        assert!(profile_deny_reason(SafetyProfile::Plan, "bash").is_some());
        assert!(profile_deny_reason(SafetyProfile::Plan, "write").is_some());
        assert!(profile_deny_reason(SafetyProfile::Plan, "edit").is_some());
        assert!(profile_deny_reason(SafetyProfile::Plan, "read").is_none());
        assert!(profile_deny_reason(SafetyProfile::Plan, "grep").is_none());
        assert!(profile_deny_reason(SafetyProfile::Plan, "ask_user").is_none());
        assert!(profile_deny_reason(SafetyProfile::Plan, "todo").is_none());
    }

    #[test]
    fn accept_edits_allows_file_and_shell() {
        assert!(profile_deny_reason(SafetyProfile::AcceptEdits, "write").is_none());
        assert!(profile_deny_reason(SafetyProfile::AcceptEdits, "bash").is_none());
        assert!(profile_skips_approval(SafetyProfile::AcceptEdits, "write"));
        assert!(!profile_skips_approval(SafetyProfile::AcceptEdits, "bash"));
    }

    #[test]
    fn auto_approve_skips_all_approval() {
        assert!(profile_deny_reason(SafetyProfile::AutoApprove, "bash").is_none());
        assert!(profile_skips_approval(SafetyProfile::AutoApprove, "bash"));
        assert!(profile_skips_approval(SafetyProfile::AutoApprove, "write"));
    }

    #[test]
    fn profile_hook_denies_plan_bash() {
        let h = profile_hook(SafetyProfile::Plan);
        let r = h("1", "bash", &serde_json::json!({"command": "ls"}));
        assert!(r.unwrap().contains("plan"));
        assert!(h("1", "read", &serde_json::json!({"path": "a"})).is_none());
    }
}
