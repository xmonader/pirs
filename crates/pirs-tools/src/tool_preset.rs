//! Named tool-policy presets for hybrid / weak-exec experiments and product UX.
//!
//! Maps a small vocabulary onto the existing permission ladder, safety profile,
//! tool-diet, sequential execution, and optional tool-call budgets — without a
//! second parallel policy system.

use crate::permission_mode::PermissionMode;
use crate::safety_profile::SafetyProfile;

/// Named tool-policy preset (CLI: `--tool-preset`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolPreset {
    /// Full tool surface (default product path).
    Full,
    /// Edit + test oriented: file mutations allowed, shell still gated by
    /// workspace-write (bash denied unless raised). Sequential + tool-diet.
    EditTest,
    /// Observation only — no file mutations, no shell.
    ReadOnly,
    /// Minimal / no-tools: cannot complete a multi-step edit-and-test loop
    /// (`max_tool_calls = 0`).
    NoTools,
}

/// Concrete settings a preset expands into.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolPresetConfig {
    pub permission_mode: PermissionMode,
    pub agent_profile: SafetyProfile,
    pub tool_diet: bool,
    pub sequential: bool,
    pub max_tool_calls: Option<usize>,
}

impl ToolPreset {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "full" | "all" | "default" => Some(Self::Full),
            "edit-test" | "edit_test" | "edittest" | "edit+test" => Some(Self::EditTest),
            "read-only" | "readonly" | "ro" | "plan" => Some(Self::ReadOnly),
            "no-tools" | "notools" | "none" | "minimal" | "no_tools" => Some(Self::NoTools),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::EditTest => "edit-test",
            Self::ReadOnly => "read-only",
            Self::NoTools => "no-tools",
        }
    }

    /// Expand into permission / profile / diet / budget knobs.
    pub fn config(self) -> ToolPresetConfig {
        match self {
            Self::Full => ToolPresetConfig {
                permission_mode: PermissionMode::DangerFullAccess,
                agent_profile: SafetyProfile::Default,
                tool_diet: false,
                sequential: false,
                max_tool_calls: None,
            },
            Self::EditTest => ToolPresetConfig {
                // File edits allowed; bash still denied under workspace-write.
                // run_tests is classified as shell → denied; agents use edit +
                // harness --verify for the test oracle (experiment-friendly).
                permission_mode: PermissionMode::WorkspaceWrite,
                agent_profile: SafetyProfile::AcceptEdits,
                tool_diet: true,
                sequential: true,
                max_tool_calls: None,
            },
            Self::ReadOnly => ToolPresetConfig {
                permission_mode: PermissionMode::ReadOnly,
                agent_profile: SafetyProfile::Plan,
                tool_diet: true,
                sequential: true,
                max_tool_calls: None,
            },
            Self::NoTools => ToolPresetConfig {
                permission_mode: PermissionMode::ReadOnly,
                agent_profile: SafetyProfile::Plan,
                tool_diet: true,
                sequential: true,
                max_tool_calls: Some(0),
            },
        }
    }

    /// Whether this preset may mutate workspace files (edit/write).
    pub fn allows_mutation(self) -> bool {
        matches!(self, Self::Full | Self::EditTest)
    }

    /// Whether a multi-step edit-and-test loop is possible under this preset.
    pub fn allows_edit_and_test_loop(self) -> bool {
        match self {
            Self::Full => true,
            // Edit-test can edit; tests usually via harness verify, not bash.
            Self::EditTest => true,
            Self::ReadOnly | Self::NoTools => false,
        }
    }
}

/// Apply a preset onto mutable CLI-style knobs. Only fills fields that are still
/// at their defaults so explicit user flags win.
pub fn apply_tool_preset(
    preset: ToolPreset,
    permission_mode: &mut Option<String>,
    agent_profile: &mut String,
    tool_diet: &mut bool,
    sequential: &mut bool,
    max_tool_calls: &mut Option<usize>,
) {
    let cfg = preset.config();
    if permission_mode.is_none() {
        *permission_mode = Some(cfg.permission_mode.name().to_string());
    }
    if agent_profile == "default" {
        *agent_profile = cfg.agent_profile.name().to_string();
    }
    if !*tool_diet {
        *tool_diet = cfg.tool_diet;
    }
    if !*sequential {
        *sequential = cfg.sequential;
    }
    if max_tool_calls.is_none() {
        *max_tool_calls = cfg.max_tool_calls;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission_mode::permission_deny_reason;

    #[test]
    fn parse_aliases() {
        assert_eq!(ToolPreset::parse("full"), Some(ToolPreset::Full));
        assert_eq!(ToolPreset::parse("edit-test"), Some(ToolPreset::EditTest));
        assert_eq!(ToolPreset::parse("edit+test"), Some(ToolPreset::EditTest));
        assert_eq!(ToolPreset::parse("read-only"), Some(ToolPreset::ReadOnly));
        assert_eq!(ToolPreset::parse("no-tools"), Some(ToolPreset::NoTools));
        assert_eq!(ToolPreset::parse("minimal"), Some(ToolPreset::NoTools));
        assert_eq!(ToolPreset::parse("bogus"), None);
    }

    #[test]
    fn read_only_cannot_mutate() {
        let c = ToolPreset::ReadOnly.config();
        assert!(!ToolPreset::ReadOnly.allows_mutation());
        assert!(!ToolPreset::ReadOnly.allows_edit_and_test_loop());
        assert!(permission_deny_reason(c.permission_mode, "edit").is_some());
        assert!(permission_deny_reason(c.permission_mode, "bash").is_some());
        assert!(permission_deny_reason(c.permission_mode, "read").is_none());
    }

    #[test]
    fn no_tools_zero_budget_and_no_mutation() {
        let c = ToolPreset::NoTools.config();
        assert_eq!(c.max_tool_calls, Some(0));
        assert!(!ToolPreset::NoTools.allows_mutation());
        assert!(!ToolPreset::NoTools.allows_edit_and_test_loop());
        assert!(permission_deny_reason(c.permission_mode, "edit").is_some());
    }

    #[test]
    fn edit_test_allows_edit_blocks_bash() {
        let c = ToolPreset::EditTest.config();
        assert!(ToolPreset::EditTest.allows_mutation());
        assert!(ToolPreset::EditTest.allows_edit_and_test_loop());
        assert!(permission_deny_reason(c.permission_mode, "edit").is_none());
        assert!(permission_deny_reason(c.permission_mode, "bash").is_some());
        assert!(c.tool_diet && c.sequential);
    }

    #[test]
    fn full_allows_bash_and_edit() {
        let c = ToolPreset::Full.config();
        assert!(permission_deny_reason(c.permission_mode, "bash").is_none());
        assert!(permission_deny_reason(c.permission_mode, "edit").is_none());
        assert!(ToolPreset::Full.allows_edit_and_test_loop());
    }

    #[test]
    fn apply_does_not_override_explicit_flags() {
        let mut perm = Some("read-only".into());
        let mut profile = "plan".into();
        let mut diet = true;
        let mut seq = true;
        let mut max = Some(5usize);
        apply_tool_preset(
            ToolPreset::Full,
            &mut perm,
            &mut profile,
            &mut diet,
            &mut seq,
            &mut max,
        );
        assert_eq!(perm.as_deref(), Some("read-only"));
        assert_eq!(profile, "plan");
        assert_eq!(max, Some(5));
    }

    #[test]
    fn apply_fills_defaults_from_preset() {
        let mut perm = None;
        let mut profile = "default".into();
        let mut diet = false;
        let mut seq = false;
        let mut max = None;
        apply_tool_preset(
            ToolPreset::NoTools,
            &mut perm,
            &mut profile,
            &mut diet,
            &mut seq,
            &mut max,
        );
        assert_eq!(perm.as_deref(), Some("read-only"));
        assert_eq!(profile, "plan");
        assert!(diet && seq);
        assert_eq!(max, Some(0));
    }
}
