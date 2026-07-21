//! Ordered permission ladder (Claude/Cline/Claw-Code style).
//!
//! Composes with [`crate::SafetyProfile`] and approval ask/yolo:
//! - `read-only` — only observation tools
//! - `workspace-write` — file mutations in workspace + reads; shell still gated
//! - `danger-full-access` — no ladder denials (approval may still apply)
//!
//! Env: `PIRS_PERMISSION_MODE` / CLI `--permission-mode`.

use pirs_agent::events::BeforeToolCallHook;
use std::sync::Arc;

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

/// Deny reason if current mode is below the tool's requirement.
pub fn permission_deny_reason(mode: PermissionMode, tool: &str) -> Option<String> {
    let need = required_mode_for_tool(tool);
    if mode >= need {
        None
    } else {
        Some(format!(
            "tool `{tool}` requires permission mode `{}` (current: `{}`); \
             raise with --permission-mode / PIRS_PERMISSION_MODE or use plan→act",
            need.name(),
            mode.name()
        ))
    }
}

pub fn permission_hook(mode: PermissionMode) -> BeforeToolCallHook {
    Arc::new(move |_id, tool, _args| permission_deny_reason(mode, tool))
}

#[cfg(test)]
mod tests {
    use super::*;

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
}
