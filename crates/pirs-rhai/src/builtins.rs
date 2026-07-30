//! The built-in loop strategies, as embedded Rhai scripts.
//!
//! These used to be Rust constructors in `pirs-agent`. They are policy, not
//! mechanism, so they live here as data: the canonical `.rhai` scripts are
//! compiled into the binary with `include_str!` and parsed on first use. Core
//! (`pirs-agent`) keeps only the engine; the built-in *content* is here alongside
//! discovery, which already resolves user scripts. A user file at
//! `.pirs/strategies/<name>.rhai` shadows the built-in of the same name.

use std::collections::HashMap;
use std::sync::OnceLock;

use pirs_agent::strategy::Strategy;

use crate::strategy_script::load_strategy_str;

const MONOLITHIC: &str = include_str!("../builtins/monolithic.rhai");
const PLAN_EXEC: &str = include_str!("../builtins/plan-exec.rhai");
const PLAN_CRITIC_EXEC: &str = include_str!("../builtins/plan-critic-exec.rhai");
const WIDE_PLAN_EXEC: &str = include_str!("../builtins/wide-plan-exec.rhai");
const PLAN_EXEC_WEAK: &str = include_str!("../builtins/plan-exec-weak.rhai");
const SPARK_EMBER: &str = include_str!("../builtins/spark-ember.rhai");
const WEAK_DRIVE: &str = include_str!("../builtins/weak-drive.rhai");

/// Primary product strategies (strong-plan/weak-exec pitch). Others remain
/// available by name but are not the front-door set.
const SOURCES: &[(&str, &str)] = &[
    ("monolithic", MONOLITHIC),
    ("plan-exec", PLAN_EXEC),
    ("plan-critic-exec", PLAN_CRITIC_EXEC),
    // Secondary / experimental — still loadable, not the product focus:
    ("wide-plan-exec", WIDE_PLAN_EXEC),
    ("plan-exec-weak", PLAN_EXEC_WEAK),
    // soulrs dual-mode analogue (spark explore fan → ember code):
    ("spark-ember", SPARK_EMBER),
    // weak drives; strong only at plan+review checkpoints (multi-provider hybrid):
    ("weak-drive", WEAK_DRIVE),
];

/// Names of the built-in strategies, in display order.
pub fn builtin_names() -> Vec<&'static str> {
    SOURCES.iter().map(|(name, _)| *name).collect()
}

/// The three strategies the product is built around.
pub fn primary_names() -> &'static [&'static str] {
    &["monolithic", "plan-exec", "plan-critic-exec"]
}

/// The parsed built-ins, keyed by name. Parsed once; a parse failure is a bug in
/// a shipped script and panics loudly (also caught by `all_builtins_parse`).
fn registry() -> &'static HashMap<&'static str, Strategy> {
    static CACHE: OnceLock<HashMap<&'static str, Strategy>> = OnceLock::new();
    CACHE.get_or_init(|| {
        SOURCES
            .iter()
            .map(|(name, src)| {
                let strat = load_strategy_str(src, name).unwrap_or_else(|e| {
                    panic!("built-in strategy {name:?} failed to parse: {e:#}")
                });
                (*name, strat)
            })
            .collect()
    })
}

/// Canonicalize user-facing aliases to registry keys.
/// `plan-exec-critic` is accepted as a synonym for `plan-critic-exec`.
/// `dual` / `soul-dual` map to `spark-ember` (soulrs taskRouter analogue).
/// `advisor` / `weak-strong` / `advise-exec` map to `weak-drive`.
pub fn canonicalize_name(name: &str) -> &str {
    match name {
        "plan-exec-critic" | "plan_critic_exec" => "plan-critic-exec",
        "plan_exec" => "plan-exec",
        "dual" | "soul-dual" | "spark_ember" | "soulrs-dual" => "spark-ember",
        "advisor" | "weak-strong" | "advise-exec" | "weak_drive" | "weak-advisor" => "weak-drive",
        other => other,
    }
}

/// Look up a built-in strategy by name (aliases accepted).
pub fn builtin(name: &str) -> Option<Strategy> {
    registry().get(canonicalize_name(name)).cloned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_agent::strategy::{Step, ToolScope};

    #[test]
    fn all_builtins_parse_and_are_named_consistently() {
        for name in builtin_names() {
            let s = builtin(name).unwrap_or_else(|| panic!("{name} missing"));
            assert_eq!(s.name, name, "script name must match its registry key");
            assert!(!s.steps.is_empty(), "{name} is empty");
        }
        assert!(builtin("does-not-exist").is_none());
    }

    #[test]
    fn monolithic_is_a_single_persistent_full_phase() {
        let s = builtin("monolithic").unwrap();
        assert!(
            s.persist_across_attempts,
            "monolithic persists across attempts"
        );
        assert_eq!(s.steps.len(), 1);
        match &s.steps[0] {
            Step::Solo(p) => assert_eq!(p.scope, ToolScope::Full),
            Step::Fan { .. } => panic!("monolithic is a single solo phase"),
        }
    }

    #[test]
    fn plan_exec_is_readonly_plan_then_full_exec() {
        let s = builtin("plan-exec").unwrap();
        assert!(!s.persist_across_attempts);
        assert_eq!(s.steps.len(), 2);
        match (&s.steps[0], &s.steps[1]) {
            (Step::Solo(plan), Step::Solo(exec)) => {
                assert_eq!(plan.scope, ToolScope::ReadOnly);
                assert_eq!(exec.scope, ToolScope::Full);
                assert!(plan.prompt.contains("{issue}"));
                assert!(exec.prompt.contains("{prev}"));
            }
            _ => panic!("plan-exec is two solo phases"),
        }
    }

    #[test]
    fn plan_critic_exec_has_three_phases() {
        let s = builtin("plan-critic-exec").unwrap();
        assert_eq!(s.steps.len(), 3);
    }

    #[test]
    fn plan_exec_critic_alias_resolves() {
        let a = builtin("plan-exec-critic").expect("alias");
        let b = builtin("plan-critic-exec").expect("canonical");
        assert_eq!(a.name, b.name);
        assert_eq!(a.steps.len(), 3);
        assert_eq!(canonicalize_name("plan-exec-critic"), "plan-critic-exec");
    }

    #[test]
    fn primary_names_are_the_product_three() {
        assert_eq!(
            primary_names(),
            &["monolithic", "plan-exec", "plan-critic-exec"]
        );
    }

    #[test]
    fn wide_plan_exec_fans_out_three_readonly_then_full_exec() {
        let s = builtin("wide-plan-exec").unwrap();
        assert_eq!(s.steps.len(), 2);
        match &s.steps[0] {
            Step::Fan { branches, .. } => {
                assert_eq!(branches.len(), 3, "three parallel planners");
                assert!(branches.iter().all(|b| b.scope == ToolScope::ReadOnly));
                // Each branch takes a distinct investigative angle.
                assert!(branches[0].prompt.contains("failing assertion"));
                assert!(branches[2].prompt.contains("boundary/edge"));
            }
            Step::Solo(_) => panic!("first step must be a fan-out"),
        }
        match &s.steps[1] {
            Step::Solo(p) => assert_eq!(p.scope, ToolScope::Full),
            Step::Fan { .. } => panic!("second step is the solo executor"),
        }
    }

    #[test]
    fn plan_exec_weak_is_readonly_then_full_with_simple_prompts() {
        let s = builtin("plan-exec-weak").unwrap();
        assert!(!s.persist_across_attempts);
        assert_eq!(s.steps.len(), 2);
        match (&s.steps[0], &s.steps[1]) {
            (Step::Solo(plan), Step::Solo(exec)) => {
                assert_eq!(plan.scope, ToolScope::ReadOnly);
                assert_eq!(exec.scope, ToolScope::Full);
                assert!(
                    plan.system.contains("short fix plan")
                        || plan.system.contains("SELF-CONTAINED"),
                    "weak plan system should be simple: {}",
                    plan.system
                );
                assert!(
                    exec.system.contains("One edit")
                        || exec.system.contains("one edit")
                        || exec.system.contains("After each"),
                    "weak exec should stress step-by-step: {}",
                    exec.system
                );
            }
            _ => panic!("plan-exec-weak is two solo phases"),
        }
    }

    #[test]
    fn spark_ember_fans_two_sparks_then_ember_exec() {
        let s = builtin("spark-ember").unwrap();
        assert!(!s.persist_across_attempts);
        assert_eq!(s.steps.len(), 2);
        match &s.steps[0] {
            Step::Fan { branches, .. } => {
                assert_eq!(branches.len(), 2, "two parallel spark explorers");
                assert!(branches.iter().all(|b| b.scope == ToolScope::ReadOnly));
                assert!(
                    branches[0].system.contains("SPARK") || branches[0].system.contains("explore"),
                    "spark system: {}",
                    branches[0].system
                );
            }
            Step::Solo(_) => panic!("first step must be spark fan-out"),
        }
        match &s.steps[1] {
            Step::Solo(p) => {
                assert_eq!(p.scope, ToolScope::Full);
                assert!(
                    p.system.contains("EMBER") || p.system.contains("code"),
                    "ember system: {}",
                    p.system
                );
                assert!(p.prompt.contains("{prev}"));
            }
            Step::Fan { .. } => panic!("second step is solo ember"),
        }
    }

    #[test]
    fn dual_alias_resolves_to_spark_ember() {
        let a = builtin("dual").expect("dual alias");
        let b = builtin("soul-dual").expect("soul-dual alias");
        let c = builtin("spark-ember").expect("canonical");
        assert_eq!(a.name, c.name);
        assert_eq!(b.name, c.name);
        assert_eq!(canonicalize_name("dual"), "spark-ember");
        assert_eq!(canonicalize_name("soulrs-dual"), "spark-ember");
    }

    #[test]
    fn weak_drive_is_plan_exec_review_fixup() {
        let s = builtin("weak-drive").unwrap();
        assert!(!s.persist_across_attempts);
        assert!(
            s.hybrid,
            "weak-drive enables agentpy hybrid thrash→advisor on full phases"
        );
        assert_eq!(s.steps.len(), 4, "plan → exec → review → fixup");
        match (&s.steps[0], &s.steps[1], &s.steps[2], &s.steps[3]) {
            (Step::Solo(plan), Step::Solo(exec), Step::Solo(review), Step::Solo(fixup)) => {
                assert_eq!(plan.scope, ToolScope::ReadOnly);
                assert_eq!(exec.scope, ToolScope::Full);
                assert_eq!(review.scope, ToolScope::ReadOnly);
                assert_eq!(fixup.scope, ToolScope::Full);
                assert!(
                    plan.system.contains("STRONG") || plan.system.contains("advisor"),
                    "plan system: {}",
                    plan.system
                );
                assert!(
                    exec.system.contains("WEAK") || exec.system.contains("executor"),
                    "exec system: {}",
                    exec.system
                );
                assert!(
                    review.system.contains("APPROVE") && review.system.contains("REVISE"),
                    "review must gate on APPROVE/REVISE: {}",
                    review.system
                );
                assert!(
                    review.prompt.contains("{prev_0}"),
                    "review must keep the original plan via prev_0: {}",
                    review.prompt
                );
                assert_eq!(
                    fixup.skip_if_prev_prefix.as_deref(),
                    Some("APPROVE"),
                    "fixup must skip free on APPROVE"
                );
                assert!(
                    fixup.prompt.contains("{prev}"),
                    "fixup applies review: {}",
                    fixup.system
                );
            }
            _ => panic!("weak-drive must be four solo phases"),
        }
    }

    #[test]
    fn advisor_alias_resolves_to_weak_drive() {
        let a = builtin("advisor").expect("advisor alias");
        let b = builtin("weak-strong").expect("weak-strong alias");
        let c = builtin("weak-drive").expect("canonical");
        assert_eq!(a.name, c.name);
        assert_eq!(b.name, c.name);
        assert_eq!(canonicalize_name("advisor"), "weak-drive");
        assert_eq!(canonicalize_name("advise-exec"), "weak-drive");
    }
}
