//! Every profile shipped in the repo's `.pirs/profiles/` must load and resolve
//! into a runnable role (`pirs --profile <name>`).

use std::path::PathBuf;

use pirs_agent::strategy::Step;
use pirs_rhai::profile_script::load_profile_file;

/// The repo's shipped profiles live in `<workspace>/.pirs/profiles`.
fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.pirs/profiles")
}

#[test]
fn every_shipped_profile_loads_and_resolves() {
    let dir = profiles_dir();
    let mut loaded = 0;
    for entry in std::fs::read_dir(&dir).expect("profiles dir exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rhai") {
            continue;
        }
        let profile = load_profile_file(&path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e:#}", path.display()));
        let resolved = profile.resolved_strategy();
        assert!(!resolved.steps.is_empty(), "{} is empty", path.display());
        loaded += 1;
    }
    assert!(loaded >= 1, "expected at least one shipped profile");
}

#[test]
fn weak_profile_loads_and_uses_plan_exec_weak() {
    let profile = load_profile_file(&profiles_dir().join("weak.rhai")).expect("weak loads");
    assert_eq!(profile.name, "weak");
    assert_eq!(profile.strategy.name, "plan-exec-weak");
    assert!(profile.persona.is_some());
    let resolved = profile.resolved_strategy();
    assert_eq!(resolved.steps.len(), 2);
    // Built-in name also resolves without a file.
    let builtin = pirs_rhai::discover::builtin_profile("weak").expect("builtin weak");
    assert_eq!(builtin.strategy.name, "plan-exec-weak");
}

#[test]
fn security_reviewer_bakes_in_persona_model_and_denies_shell() {
    let profile = load_profile_file(&profiles_dir().join("security-reviewer.rhai"))
        .expect("security-reviewer loads");
    assert!(!profile.tools.permits("bash"), "shell must be denied");
    assert!(profile.tools.permits("read"));

    let resolved = profile.resolved_strategy();
    for step in &resolved.steps {
        if let Step::Solo(phase) = step {
            assert!(
                phase.system.to_lowercase().contains("security"),
                "persona not stamped onto phase: {}",
                phase.system
            );
            // The role pins no model: every phase inherits the run's --model, so
            // the profile is provider-agnostic.
            assert_eq!(
                phase.model, None,
                "profile must not pin a provider-specific model"
            );
        }
    }
}
