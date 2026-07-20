//! Pure decisions for the `--weak` preset.
//!
//! Kept free of clap/TUI so unit tests can exercise composition without a full
//! interactive surface. CLI wiring in `main` only maps flags into
//! [`WeakComposeInput`] and applies [`WeakComposeResult`].

/// Pack names loaded by the bundled weak stack, in load order.
/// First loaded registers first; later registrations win on tool name collisions.
/// Project/user extensions load *after* this list (see main).
pub const WEAK_BUNDLED_PACKS: &[&str] = &[
    "weak-model",
    "context-janitor",
    "env-doctor",
    "goal",
];

/// Inputs that affect how `--weak` rewrites runtime flags.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeakComposeInput {
    /// True when the user passed a one-shot prompt (non-interactive path).
    pub has_prompt: bool,
    pub strategy: Option<String>,
    pub profile: Option<String>,
    pub verify: Option<String>,
    pub max_retries: u32,
    pub tool_diet: bool,
    pub sequential: bool,
}

/// Result of applying the weak preset to an input.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WeakComposeResult {
    pub tool_diet: bool,
    pub sequential: bool,
    pub max_retries: u32,
    pub strategy: Option<String>,
    pub profile: Option<String>,
    pub verify: Option<String>,
    /// Human-readable note for stderr (selection or skip). None when verify was already set.
    pub auto_verify_note: Option<String>,
    /// Bundled pack stems loaded under `--weak` (deterministic order).
    pub bundled_packs: &'static [&'static str],
}

/// Detected ecosystem name + shell command from marker-file detection.
pub type DetectedVerify = (String, String);

/// Apply `--weak` composition rules.
///
/// - Always: tool_diet, sequential, max_retries floor of 3, bundled pack list.
/// - Strategy: default `plan-exec-weak` only for one-shot when neither strategy
///   nor profile was set.
/// - Auto-verify: only when verify unset, one-shot, and strategy/profile will run;
///   uses `detected` when present, otherwise records a skip note (never invents a command).
pub fn apply_weak_preset(
    input: WeakComposeInput,
    detected: Option<DetectedVerify>,
) -> WeakComposeResult {
    // Weak forces diet/sequential on; input flags are ignored (CLI may already
    // have set them; composition always yields true under --weak).
    let _ = (input.tool_diet, input.sequential);
    let tool_diet = true;
    let sequential = true;

    let max_retries = input.max_retries.max(3);

    let mut strategy = input.strategy;
    let profile = input.profile;
    if strategy.is_none() && profile.is_none() && input.has_prompt {
        strategy = Some("plan-exec-weak".into());
    }

    let mut verify = input.verify;
    let mut auto_verify_note = None;
    let strategy_or_profile = strategy.is_some() || profile.is_some();
    if verify.is_none() && input.has_prompt && strategy_or_profile {
        match detected {
            Some((eco, cmd)) => {
                auto_verify_note = Some(format!("[weak auto-verify: {eco} → `{cmd}`]"));
                verify = Some(cmd);
            }
            None => {
                auto_verify_note = Some(
                    "[weak auto-verify: skipped — no test ecosystem detected \
                     (Cargo.toml / go.mod / package.json / pytest markers / Makefile)]"
                        .into(),
                );
            }
        }
    }

    WeakComposeResult {
        tool_diet,
        sequential,
        max_retries,
        strategy,
        profile,
        verify,
        auto_verify_note,
        bundled_packs: WEAK_BUNDLED_PACKS,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn base() -> WeakComposeInput {
        WeakComposeInput {
            has_prompt: true,
            strategy: None,
            profile: None,
            verify: None,
            max_retries: 1,
            tool_diet: false,
            sequential: false,
        }
    }

    #[test]
    fn forces_diet_sequential_and_retry_floor() {
        let out = apply_weak_preset(base(), None);
        assert!(out.tool_diet);
        assert!(out.sequential);
        assert_eq!(out.max_retries, 3);
        assert_eq!(out.strategy.as_deref(), Some("plan-exec-weak"));
    }

    #[test]
    fn interactive_without_prompt_skips_default_strategy_and_auto_verify() {
        let mut inp = base();
        inp.has_prompt = false;
        let out = apply_weak_preset(inp, Some(("rust".into(), "cargo test".into())));
        assert!(out.strategy.is_none());
        assert!(out.verify.is_none());
        assert!(out.auto_verify_note.is_none());
    }

    #[test]
    fn explicit_strategy_and_verify_win() {
        let mut inp = base();
        inp.strategy = Some("monolithic".into());
        inp.verify = Some("make check".into());
        inp.max_retries = 5;
        let out = apply_weak_preset(inp, Some(("rust".into(), "cargo test".into())));
        assert_eq!(out.strategy.as_deref(), Some("monolithic"));
        assert_eq!(out.verify.as_deref(), Some("make check"));
        assert!(out.auto_verify_note.is_none());
        assert_eq!(out.max_retries, 5);
    }

    #[test]
    fn auto_verify_selects_detected_command() {
        let out = apply_weak_preset(base(), Some(("rust".into(), "cargo test".into())));
        assert_eq!(out.verify.as_deref(), Some("cargo test"));
        let note = out.auto_verify_note.expect("note");
        assert!(note.contains("rust"));
        assert!(note.contains("cargo test"));
    }

    #[test]
    fn auto_verify_skips_cleanly_without_ecosystem() {
        let out = apply_weak_preset(base(), None);
        assert!(out.verify.is_none());
        let note = out.auto_verify_note.expect("skip note");
        assert!(note.contains("skipped"));
        assert!(note.contains("Cargo.toml"));
    }

    #[test]
    fn bundled_pack_list_matches_weak_stack() {
        let out = apply_weak_preset(base(), None);
        assert_eq!(
            out.bundled_packs,
            ["weak-model", "context-janitor", "env-doctor", "goal"]
        );
    }

    #[test]
    fn profile_without_strategy_still_gets_auto_verify() {
        let mut inp = base();
        inp.profile = Some("weak".into());
        let out = apply_weak_preset(inp, Some(("python".into(), "pytest -q".into())));
        assert_eq!(out.verify.as_deref(), Some("pytest -q"));
        assert_eq!(out.profile.as_deref(), Some("weak"));
        assert!(out.strategy.is_none());
    }
}
