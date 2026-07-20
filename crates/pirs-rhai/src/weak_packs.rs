//! Bundled extension packs loaded by `pirs --weak`.
//!
//! These are the same sources as `extensions/*.rhai` in the repo catalog,
//! embedded so `--weak` works without the user copying files into
//! `.pirs/extensions/`. Project/user extensions load *after* this stack and
//! can override tools by name (last registration wins).
//!
//! ## Load order (deterministic)
//!
//! 1. `weak-model` — thrash/stop-gate/plan pin (`update_plan` tool)
//! 2. `context-janitor` — shrink stale tool outputs
//! 3. `env-doctor` — missing toolchain blocks
//! 4. `goal` — session goal pin (uses `[SESSION GOAL]`, not system-reminder)
//!
//! `weak-model` loads first so later packs can override its tools if they
//! register the same name; project extensions still load after the whole stack.

/// Pack stems in load order (stable contract for docs and tests).
pub const BUNDLED_ORDER: &[&str] = &[
    "weak-model",
    "context-janitor",
    "env-doctor",
    "goal",
];

/// `(display_name, source)` for every pack auto-loaded under `--weak`.
/// Order matches [`BUNDLED_ORDER`].
pub const BUNDLED: &[(&str, &str)] = &[
    (
        "bundled:weak-model.rhai",
        include_str!("../../../extensions/weak-model.rhai"),
    ),
    (
        "bundled:context-janitor.rhai",
        include_str!("../../../extensions/context-janitor.rhai"),
    ),
    (
        "bundled:env-doctor.rhai",
        include_str!("../../../extensions/env-doctor.rhai"),
    ),
    (
        "bundled:goal.rhai",
        include_str!("../../../extensions/goal.rhai"),
    ),
];

/// Load every bundled weak-model pack into `host`. Errors are pushed to
/// `host.load_errors` rather than aborting — a single broken pack should not
/// disable the rest of the session.
pub fn load_into(host: &mut crate::ExtensionHost) {
    for (name, src) in BUNDLED {
        if let Err(e) = host.load_source(src, (*name).to_string()) {
            host.load_errors
                .push(format!("{name}: failed to load bundled weak pack: {e:#}"));
        }
    }
}

/// Built-in profile source for `--profile weak` when no file is found.
pub const WEAK_PROFILE: &str = include_str!("../builtins/weak.profile.rhai");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn all_bundled_packs_load() {
        let mut host = crate::ExtensionHost::new();
        load_into(&mut host);
        assert!(
            host.load_errors.is_empty(),
            "bundled packs must load cleanly: {:?}",
            host.load_errors
        );
        let names = host.extension_names();
        assert!(
            names.iter().any(|n| n.contains("weak-model")),
            "expected weak-model in {names:?}"
        );
    }

    #[test]
    fn bundled_order_matches_sources_and_is_deterministic() {
        assert_eq!(BUNDLED.len(), BUNDLED_ORDER.len());
        for (i, stem) in BUNDLED_ORDER.iter().enumerate() {
            assert!(
                BUNDLED[i].0.contains(stem),
                "BUNDLED[{i}] display name {:?} must contain stem {stem}",
                BUNDLED[i].0
            );
        }
        // weak-model first: owns update_plan; later packs must not silently
        // replace load order without updating BUNDLED_ORDER + catalog docs.
        assert_eq!(BUNDLED_ORDER[0], "weak-model");
        assert_eq!(BUNDLED_ORDER[3], "goal");
    }

    #[test]
    fn load_into_registers_extensions_in_bundled_order() {
        let mut host = crate::ExtensionHost::new();
        load_into(&mut host);
        let names = host.extension_names();
        let mut positions = Vec::new();
        for stem in BUNDLED_ORDER {
            let pos = names
                .iter()
                .position(|n| n.contains(stem))
                .unwrap_or_else(|| panic!("missing {stem} in {names:?}"));
            positions.push(pos);
        }
        let mut sorted = positions.clone();
        sorted.sort();
        assert_eq!(
            positions, sorted,
            "extensions must load in BUNDLED_ORDER; got positions {positions:?} names {names:?}"
        );
        // weak-model registers update_plan
        let host = std::sync::Arc::new(host);
        let tools: Vec<_> = host.tools().iter().map(|t| t.name().to_string()).collect();
        assert!(
            tools.iter().any(|n| n == "update_plan"),
            "weak stack must expose update_plan: {tools:?}"
        );
    }

    #[test]
    fn weak_profile_script_parses() {
        let p = crate::profile_script::load_profile_str(WEAK_PROFILE, "weak").unwrap();
        assert_eq!(p.name, "weak");
        assert_eq!(p.strategy.name, "plan-exec-weak");
        assert!(p.persona.is_some());
    }
}
