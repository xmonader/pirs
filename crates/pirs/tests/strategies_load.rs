//! Every strategy shipped in the repo's `.pirs/strategies/` must actually load.
//! These are first-class, name-discoverable strategies (`pirs --strategy <name>`)
//! and the working reference for the strategy DSL; if a schema change breaks one,
//! this test fails before it breaks in someone's hands.

use std::path::PathBuf;

use pirs_rhai::strategy_script::load_strategy_file;

/// The repo's shipped strategies live in `<workspace>/.pirs/strategies`.
fn strategies_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../.pirs/strategies")
}

#[test]
fn every_shipped_strategy_script_loads() {
    let dir = strategies_dir();
    let mut loaded = 0;
    for entry in std::fs::read_dir(&dir).expect("strategies dir exists") {
        let path = entry.unwrap().path();
        if path.extension().and_then(|e| e.to_str()) != Some("rhai") {
            continue;
        }
        let strat = load_strategy_file(&path)
            .unwrap_or_else(|e| panic!("failed to load {}: {e:#}", path.display()));
        assert!(
            !strat.steps.is_empty(),
            "{} produced an empty strategy",
            path.display()
        );
        loaded += 1;
    }
    assert!(
        loaded >= 3,
        "expected at least 3 shipped strategies, saw {loaded}"
    );
}

#[test]
fn general_wide_plan_exec_script_fans_out_then_executes() {
    use pirs_agent::strategy::{Step, ToolScope};

    let path = strategies_dir().join("general-wide-plan-exec.rhai");
    let strat = load_strategy_file(&path).expect("general-wide-plan-exec loads");
    assert_eq!(strat.steps.len(), 2);
    match &strat.steps[0] {
        Step::Fan { branches, .. } => {
            assert_eq!(branches.len(), 3, "three parallel planners");
            assert!(
                branches.iter().all(|b| b.scope == ToolScope::ReadOnly),
                "fan-out branches must be read-only"
            );
        }
        Step::Solo(_) => panic!("first step must be a fan-out"),
    }
    match &strat.steps[1] {
        Step::Solo(p) => assert_eq!(p.scope, ToolScope::Full, "executor is full-scope"),
        Step::Fan { .. } => panic!("second step must be the solo executor"),
    }
}
