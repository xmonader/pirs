//! Single product-facing autonomy ladder.
//!
//! Historically pirs stacked three overlapping systems:
//! - approval: auto | ask | yolo
//! - permission: read-only | workspace-write | danger-full-access
//! - agent-profile: default | plan | accept-edits | auto-approve
//!
//! Users saw "yolo" while bash still died on the permission ladder. This module
//! is the **one** knob: plan → edit → full. It expands into the internal
//! permission + profile + approval values so gates stay compatible.

use crate::permission_mode::PermissionMode;
use crate::safety_profile::SafetyProfile;

/// Product autonomy (what the agent may do without further flags).
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Autonomy {
    /// Observe only — no writes, no shell.
    Plan = 0,
    /// Edit workspace files; shell still blocked.
    Edit = 1,
    /// Full tools + no approval prompts (true "yolo").
    Full = 2,
}

impl Autonomy {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "plan" | "read-only" | "readonly" | "ro" => Some(Self::Plan),
            // edit = workspace writes, no shell
            "edit" | "workspace-write" | "write" | "accept-edits" | "default" => Some(Self::Edit),
            // act (legacy dial) + full/yolo = everything
            "full" | "yolo" | "act" | "danger" | "danger-full-access" | "auto-approve" | "all" => {
                Some(Self::Full)
            }
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Plan => "plan",
            Self::Edit => "edit",
            Self::Full => "full",
        }
    }

    pub fn permission(self) -> PermissionMode {
        match self {
            Self::Plan => PermissionMode::ReadOnly,
            Self::Edit => PermissionMode::WorkspaceWrite,
            Self::Full => PermissionMode::DangerFullAccess,
        }
    }

    pub fn profile(self) -> SafetyProfile {
        match self {
            Self::Plan => SafetyProfile::Plan,
            // AcceptEdits: file tools skip ask prompts; shell still needs full.
            Self::Edit => SafetyProfile::AcceptEdits,
            Self::Full => SafetyProfile::AutoApprove,
        }
    }

    /// Approval string for CLI / gate (`auto` | `ask` | `yolo`).
    pub fn approval_name(self) -> &'static str {
        match self {
            Self::Plan | Self::Edit => "auto",
            Self::Full => "yolo",
        }
    }

    pub fn is_yolo(self) -> bool {
        matches!(self, Self::Full)
    }
}

/// Resolve the single autonomy level from the old stacked flags.
///
/// Precedence (highest first):
/// 1. Explicit `--autonomy` / product dial (`plan` | `edit` | `full`)
/// 2. **Yolo commitment** (`--yolo` or `--approval yolo`) → **full**
///    (beats env `PIRS_PERMISSION_MODE` / tool-preset so bash is not left blocked)
/// 3. Tool preset mapping
/// 4. Explicit permission mode
/// 5. Agent profile plan → plan
/// 6. Default → edit (workspace-write; safe coding default)
pub fn resolve_autonomy(
    mode_dial: Option<&str>,
    autonomy_flag: Option<&str>,
    tool_preset: Option<&str>,
    permission: Option<&str>,
    approval: &str,
    agent_profile: &str,
) -> Autonomy {
    if let Some(a) = autonomy_flag.and_then(Autonomy::parse) {
        return a;
    }
    if let Some(d) = mode_dial.and_then(Autonomy::parse) {
        return d;
    }
    // YOLO is a product commitment to full tools — not merely "no prompts".
    // Must beat permission env / tool-preset or users keep seeing bash blocked
    // while the UI says yolo.
    if approval.eq_ignore_ascii_case("yolo") {
        return Autonomy::Full;
    }
    if let Some(p) = tool_preset.and_then(crate::tool_preset::ToolPreset::parse) {
        return match p {
            crate::tool_preset::ToolPreset::Full => Autonomy::Full,
            crate::tool_preset::ToolPreset::EditTest => Autonomy::Edit,
            crate::tool_preset::ToolPreset::ReadOnly | crate::tool_preset::ToolPreset::NoTools => {
                Autonomy::Plan
            }
        };
    }
    if let Some(pm) = permission.and_then(PermissionMode::parse) {
        return match pm {
            PermissionMode::ReadOnly => Autonomy::Plan,
            PermissionMode::WorkspaceWrite => Autonomy::Edit,
            PermissionMode::DangerFullAccess => Autonomy::Full,
        };
    }
    if agent_profile.eq_ignore_ascii_case("plan") {
        return Autonomy::Plan;
    }
    Autonomy::Edit
}

/// Apply autonomy to live permission + env profile (single write path).
pub fn apply_autonomy(autonomy: Autonomy) {
    crate::permission_mode::set_live_permission_mode(autonomy.permission());
    std::env::set_var("PIRS_AGENT_PROFILE", autonomy.profile().name());
    std::env::set_var("PIRS_AUTONOMY", autonomy.name());
}

/// Human one-liner for status / deny hints.
pub fn autonomy_status_line(autonomy: Autonomy) -> String {
    format!(
        "autonomy: {}  (permission={}, profile={}, approval={})",
        autonomy.name(),
        autonomy.permission().name(),
        autonomy.profile().name(),
        autonomy.approval_name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_mode::permission_deny_reason;

    #[test]
    fn yolo_alone_is_full_and_allows_bash() {
        let a = resolve_autonomy(None, None, None, None, "yolo", "default");
        assert_eq!(a, Autonomy::Full);
        assert!(permission_deny_reason(a.permission(), "bash").is_none());
        assert_eq!(a.approval_name(), "yolo");
    }

    #[test]
    fn default_is_edit_blocks_bash_allows_write() {
        let a = resolve_autonomy(None, None, None, None, "auto", "default");
        assert_eq!(a, Autonomy::Edit);
        assert!(permission_deny_reason(a.permission(), "bash").is_some());
        assert!(permission_deny_reason(a.permission(), "edit").is_none());
    }

    #[test]
    fn plan_dial_and_profile_are_readonly() {
        assert_eq!(
            resolve_autonomy(Some("plan"), None, None, None, "yolo", "default"),
            Autonomy::Plan
        );
        assert_eq!(
            resolve_autonomy(None, None, None, None, "auto", "plan"),
            Autonomy::Plan
        );
        let a = Autonomy::Plan;
        assert!(permission_deny_reason(a.permission(), "write").is_some());
        assert!(permission_deny_reason(a.permission(), "read").is_none());
    }

    #[test]
    fn explicit_permission_pins_when_not_yolo() {
        assert_eq!(
            resolve_autonomy(
                None,
                None,
                None,
                Some("danger-full-access"),
                "auto",
                "default"
            ),
            Autonomy::Full
        );
        assert_eq!(
            resolve_autonomy(None, None, None, Some("read-only"), "auto", "default"),
            Autonomy::Plan
        );
    }

    #[test]
    fn yolo_beats_permission_env_and_tool_preset() {
        // The bug: PIRS_PERMISSION_MODE=workspace-write + --approval yolo
        // used to stay on edit and block bash.
        assert_eq!(
            resolve_autonomy(None, None, None, Some("workspace-write"), "yolo", "default"),
            Autonomy::Full
        );
        assert_eq!(
            resolve_autonomy(None, None, Some("edit-test"), None, "yolo", "default"),
            Autonomy::Full
        );
        assert_eq!(
            resolve_autonomy(None, None, Some("read-only"), None, "yolo", "default"),
            Autonomy::Full
        );
        assert!(permission_deny_reason(Autonomy::Full.permission(), "bash").is_none());
    }

    #[test]
    fn autonomy_flag_wins_over_yolo_and_permission() {
        // Explicit --autonomy plan still wins (user asked for read-only).
        assert_eq!(
            resolve_autonomy(None, Some("plan"), None, Some("danger-full-access"), "yolo", "default"),
            Autonomy::Plan
        );
        assert_eq!(
            resolve_autonomy(None, Some("full"), None, Some("read-only"), "ask", "plan"),
            Autonomy::Full
        );
    }

    #[test]
    fn tool_preset_maps() {
        assert_eq!(
            resolve_autonomy(None, None, Some("full"), None, "auto", "default"),
            Autonomy::Full
        );
        assert_eq!(
            resolve_autonomy(None, None, Some("edit-test"), None, "auto", "default"),
            Autonomy::Edit
        );
        assert_eq!(
            resolve_autonomy(None, None, Some("read-only"), None, "auto", "default"),
            Autonomy::Plan
        );
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(Autonomy::parse("yolo"), Some(Autonomy::Full));
        assert_eq!(Autonomy::parse("act"), Some(Autonomy::Full)); // legacy dial
        assert_eq!(Autonomy::parse("edit"), Some(Autonomy::Edit));
        assert_eq!(Autonomy::parse("plan"), Some(Autonomy::Plan));
    }
}
