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

/// `(name, embedded source)` for every built-in, in resolution/display order.
/// (`plan-oracle-exec` is intentionally not here — it is a fixed-model script
/// shipped in `.pirs/strategies/`, not a bare-name built-in.)
const SOURCES: &[(&str, &str)] = &[
    ("monolithic", MONOLITHIC),
    ("plan-exec", PLAN_EXEC),
    ("plan-critic-exec", PLAN_CRITIC_EXEC),
    ("wide-plan-exec", WIDE_PLAN_EXEC),
];

/// Names of the built-in strategies, in display order.
pub fn builtin_names() -> Vec<&'static str> {
    SOURCES.iter().map(|(name, _)| *name).collect()
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

/// Look up a built-in strategy by name.
pub fn builtin(name: &str) -> Option<Strategy> {
    registry().get(name).cloned()
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
}
