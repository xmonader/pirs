//! Every shipped example profile must load and resolve into a runnable role.

use std::path::PathBuf;

use pirs_agent::strategy::Step;
use pirs_rhai::profile_script::load_profile_file;

fn profiles_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("profiles")
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
    assert!(loaded >= 1, "expected at least one example profile");
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
            assert_eq!(phase.model.as_deref(), Some("deepseek-v4-flash"));
        }
    }
}
