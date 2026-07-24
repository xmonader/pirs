//! Ordered permission ladder (Claude/Cline/Claw-Code style).
//!
//! Composes with [`crate::SafetyProfile`] and approval ask/yolo:
//! - `read-only` — only observation tools
//! - `workspace-write` — file mutations in workspace + reads; shell still gated
//! - `danger-full-access` — no ladder denials (approval may still apply)
//!
//! Env: `PIRS_PERMISSION_MODE` / CLI `--permission-mode`.
//! Mid-session `/plan` `/act` update the live slot via [`set_live_permission_mode`].

use pirs_agent::events::BeforeToolCallHook;
use std::sync::{Arc, Mutex, OnceLock};

use crate::safety_profile::{is_file_mutation_tool, is_readonly_tool, is_shell_tool};

/// Ordered autonomy levels for tool execution.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum PermissionMode {
    ReadOnly = 0,
    WorkspaceWrite = 1,
    DangerFullAccess = 2,
}

impl PermissionMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "read-only" | "readonly" | "ro" | "plan" => Some(Self::ReadOnly),
            "workspace-write" | "workspace_write" | "write" | "default" | "accept-edits" => {
                Some(Self::WorkspaceWrite)
            }
            "danger-full-access"
            | "danger_full_access"
            | "full"
            | "danger"
            | "yolo"
            | "auto-approve" => Some(Self::DangerFullAccess),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::ReadOnly => "read-only",
            Self::WorkspaceWrite => "workspace-write",
            Self::DangerFullAccess => "danger-full-access",
        }
    }

    /// From env `PIRS_PERMISSION_MODE`, default workspace-write.
    pub fn from_env() -> Self {
        std::env::var("PIRS_PERMISSION_MODE")
            .ok()
            .and_then(|s| Self::parse(&s))
            .unwrap_or(Self::WorkspaceWrite)
    }
}

fn live_slot() -> &'static Mutex<PermissionMode> {
    static SLOT: OnceLock<Mutex<PermissionMode>> = OnceLock::new();
    SLOT.get_or_init(|| Mutex::new(PermissionMode::from_env()))
}

/// Current live permission mode (slash commands update this).
pub fn live_permission_mode() -> PermissionMode {
    *live_slot().lock().unwrap()
}

/// Set live mode and mirror to `PIRS_PERMISSION_MODE` for doctor/status.
pub fn set_live_permission_mode(mode: PermissionMode) {
    *live_slot().lock().unwrap() = mode;
    std::env::set_var("PIRS_PERMISSION_MODE", mode.name());
}

/// Install initial live mode (startup).
pub fn init_live_permission_mode(mode: PermissionMode) {
    set_live_permission_mode(mode);
}

/// Required mode for a tool name (static classification).
pub fn required_mode_for_tool(tool: &str) -> PermissionMode {
    if is_readonly_tool(tool)
        || matches!(
            tool,
            "checkpoint" | "session_rewind" | "doctor" | "audit_tail" | "git" | "pr" | "fleet"
        )
    {
        return PermissionMode::ReadOnly;
    }
    if is_file_mutation_tool(tool) || tool == "todo" {
        return PermissionMode::WorkspaceWrite;
    }
    if is_shell_tool(tool)
        || matches!(
            tool,
            "delegate" | "bash" | "run_tests" | "computer_screenshot"
        )
        || tool.starts_with("computer_")
        || tool.starts_with("browser_")
    {
        return PermissionMode::DangerFullAccess;
    }
    // Unknown tools require full access (safe default: deny under lower modes).
    PermissionMode::DangerFullAccess
}

/// Resolve live permission when approval is **yolo**.
///
/// Approval yolo only skips interactive prompts; the permission ladder is
/// separate. Users expect `yolo` to actually run bash, so when permission was
/// not explicitly set we raise the default `workspace-write` ladder to
/// `danger-full-access`. Explicit `--permission-mode` / plan dial / plan
/// profile still win.
pub fn apply_yolo_permission_default(
    approval_is_yolo: bool,
    explicit_permission: Option<PermissionMode>,
    dial_plan: bool,
    profile_is_plan: bool,
    current: PermissionMode,
) -> PermissionMode {
    if approval_is_yolo
        && explicit_permission.is_none()
        && !dial_plan
        && !profile_is_plan
        && current != PermissionMode::DangerFullAccess
    {
        PermissionMode::DangerFullAccess
    } else {
        current
    }
}

/// Deny reason if current mode is below the tool's requirement.
pub fn permission_deny_reason(mode: PermissionMode, tool: &str) -> Option<String> {
    permission_deny_reason_with_args(mode, tool, &serde_json::Value::Null)
}

/// Permission denials with tool args (action-aware for checkpoint restore / pr create).
pub fn permission_deny_reason_with_args(
    mode: PermissionMode,
    tool: &str,
    args: &serde_json::Value,
) -> Option<String> {
    // project list/packages only inspect — allow under read-only.
    // Other project actions spawn processes and need danger-full-access.
    if tool == "project" {
        if crate::safety_profile::project_action_is_readonly(args) {
            return None;
        }
        if mode < PermissionMode::DangerFullAccess {
            return Some(format!(
                "tool `project` action requires permission mode `{}` (current: `{}`); \
                 raise with --permission-mode / PIRS_PERMISSION_MODE or use plan→act",
                PermissionMode::DangerFullAccess.name(),
                mode.name()
            ));
        }
        return None;
    }
    // Mutating actions on otherwise-readonly tools need workspace-write+.
    if let Some(reason) = crate::safety_profile::mutating_action_deny(tool, args) {
        if mode < PermissionMode::WorkspaceWrite {
            return Some(reason);
        }
        // Under workspace-write+, allow (approval may still apply for pr create).
        // pr create is network-side-effectful — require danger-full-access.
        if tool == "pr"
            && args
                .get("action")
                .and_then(|v| v.as_str())
                .map(|a| a.eq_ignore_ascii_case("create"))
                .unwrap_or(false)
            && mode < PermissionMode::DangerFullAccess
        {
            return Some(format!(
                "tool `pr` action `create` requires permission mode `{}` (current: `{}`)",
                PermissionMode::DangerFullAccess.name(),
                mode.name()
            ));
        }
        return None;
    }
    let need = required_mode_for_tool(tool);
    if mode >= need {
        None
    } else {
        let need_auto = match need {
            PermissionMode::ReadOnly => "plan",
            PermissionMode::WorkspaceWrite => "edit",
            PermissionMode::DangerFullAccess => "full",
        };
        let cur_auto = match mode {
            PermissionMode::ReadOnly => "plan",
            PermissionMode::WorkspaceWrite => "edit",
            PermissionMode::DangerFullAccess => "full",
        };
        Some(format!(
            "tool `{tool}` blocked by autonomy `{cur_auto}` (need `{need_auto}`); \
             use /act or /yolo or --autonomy full  ·  permission {}→{}",
            mode.name(),
            need.name()
        ))
    }
}

/// Hook that reads the **live** permission mode on every call (plan/act slash works).
pub fn permission_hook(_initial: PermissionMode) -> BeforeToolCallHook {
    // Seed live slot if not already set by init.
    let _ = live_permission_mode();
    Arc::new(move |_id, tool, args| {
        let mode = live_permission_mode();
        permission_deny_reason_with_args(mode, tool, args)
    })
}

/// Explicit live-slot hook (same as [`permission_hook`]).
pub fn live_permission_hook() -> BeforeToolCallHook {
    permission_hook(live_permission_mode())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn yolo_lifts_default_workspace_write_to_full_access() {
        let lifted = apply_yolo_permission_default(
            true,
            None,
            false,
            false,
            PermissionMode::WorkspaceWrite,
        );
        assert_eq!(lifted, PermissionMode::DangerFullAccess);
        // Explicit pin wins.
        assert_eq!(
            apply_yolo_permission_default(
                true,
                Some(PermissionMode::WorkspaceWrite),
                false,
                false,
                PermissionMode::WorkspaceWrite,
            ),
            PermissionMode::WorkspaceWrite
        );
        // Plan dial / plan profile stay restricted.
        assert_eq!(
            apply_yolo_permission_default(
                true,
                None,
                true,
                false,
                PermissionMode::ReadOnly,
            ),
            PermissionMode::ReadOnly
        );
        assert_eq!(
            apply_yolo_permission_default(
                true,
                None,
                false,
                true,
                PermissionMode::WorkspaceWrite,
            ),
            PermissionMode::WorkspaceWrite
        );
        // Non-yolo unchanged.
        assert_eq!(
            apply_yolo_permission_default(
                false,
                None,
                false,
                false,
                PermissionMode::WorkspaceWrite,
            ),
            PermissionMode::WorkspaceWrite
        );
    }

    #[test]
    fn yolo_allows_bash_under_lifted_mode() {
        let mode = apply_yolo_permission_default(
            true,
            None,
            false,
            false,
            PermissionMode::WorkspaceWrite,
        );
        assert!(
            permission_deny_reason(mode, "bash").is_none(),
            "yolo default must not block bash"
        );
        assert!(
            permission_deny_reason(PermissionMode::WorkspaceWrite, "bash").is_some(),
            "precondition: workspace-write still blocks bash"
        );
    }

    #[test]
    fn ladder_ordering() {
        assert!(PermissionMode::ReadOnly < PermissionMode::WorkspaceWrite);
        assert!(PermissionMode::WorkspaceWrite < PermissionMode::DangerFullAccess);
    }

    #[test]
    fn read_only_blocks_write_and_bash() {
        assert!(permission_deny_reason(PermissionMode::ReadOnly, "write").is_some());
        assert!(permission_deny_reason(PermissionMode::ReadOnly, "bash").is_some());
        assert!(permission_deny_reason(PermissionMode::ReadOnly, "read").is_none());
        assert!(permission_deny_reason(PermissionMode::ReadOnly, "grep").is_none());
    }

    #[test]
    fn workspace_write_allows_office_document() {
        assert!(
            permission_deny_reason(PermissionMode::WorkspaceWrite, "office_document").is_none(),
            "office_document is a file mutation (workspace-write+)"
        );
        assert!(permission_deny_reason(PermissionMode::ReadOnly, "office_document").is_some());
    }

    #[test]
    fn workspace_write_allows_edit_blocks_bash() {
        assert!(permission_deny_reason(PermissionMode::WorkspaceWrite, "edit").is_none());
        assert!(permission_deny_reason(PermissionMode::WorkspaceWrite, "bash").is_some());
        assert!(permission_deny_reason(PermissionMode::WorkspaceWrite, "read").is_none());
    }

    #[test]
    fn full_allows_bash() {
        assert!(permission_deny_reason(PermissionMode::DangerFullAccess, "bash").is_none());
        assert!(permission_deny_reason(PermissionMode::DangerFullAccess, "write").is_none());
    }

    #[test]
    fn parse_aliases() {
        assert_eq!(PermissionMode::parse("plan"), Some(PermissionMode::ReadOnly));
        assert_eq!(
            PermissionMode::parse("workspace-write"),
            Some(PermissionMode::WorkspaceWrite)
        );
        assert_eq!(
            PermissionMode::parse("danger-full-access"),
            Some(PermissionMode::DangerFullAccess)
        );
    }

    #[test]
    fn live_mode_change_affects_hook() {
        init_live_permission_mode(PermissionMode::DangerFullAccess);
        let h = live_permission_hook();
        assert!(
            h("1", "bash", &serde_json::json!({})).is_none(),
            "full access allows bash"
        );
        set_live_permission_mode(PermissionMode::ReadOnly);
        let deny = h("1", "bash", &serde_json::json!({}));
        assert!(
            deny.as_ref().is_some_and(|s| s.contains("read-only")),
            "after /plan-style switch, bash denied: {deny:?}"
        );
        set_live_permission_mode(PermissionMode::DangerFullAccess);
        assert!(h("1", "bash", &serde_json::json!({})).is_none());
    }

    #[test]
    fn read_only_denies_checkpoint_restore_and_pr_create() {
        let restore = serde_json::json!({"action": "restore"});
        let create = serde_json::json!({"action": "create", "title": "x"});
        assert!(permission_deny_reason_with_args(
            PermissionMode::ReadOnly,
            "checkpoint",
            &restore
        )
        .is_some());
        assert!(permission_deny_reason_with_args(PermissionMode::ReadOnly, "pr", &create).is_some());
        assert!(permission_deny_reason_with_args(
            PermissionMode::ReadOnly,
            "checkpoint",
            &serde_json::json!({"action": "list"})
        )
        .is_none());
        // pr create needs danger-full-access even under workspace-write
        assert!(permission_deny_reason_with_args(
            PermissionMode::WorkspaceWrite,
            "pr",
            &create
        )
        .is_some());
        assert!(permission_deny_reason_with_args(
            PermissionMode::DangerFullAccess,
            "pr",
            &create
        )
        .is_none());
    }
}
