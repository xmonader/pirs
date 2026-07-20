//! Bundled extension packs loaded by `pirs --weak`.
//!
//! These are the same sources as `extensions/*.rhai` in the repo catalog,
//! embedded so `--weak` works without the user copying files into
//! `.pirs/extensions/`. Project/user extensions still load first and can
//! override tools by name (last registration wins).

/// `(display_name, source)` for every pack auto-loaded under `--weak`.
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
    fn weak_profile_script_parses() {
        let p = crate::profile_script::load_profile_str(WEAK_PROFILE, "weak").unwrap();
        assert_eq!(p.name, "weak");
        assert_eq!(p.strategy.name, "plan-exec-weak");
        assert!(p.persona.is_some());
    }
}
