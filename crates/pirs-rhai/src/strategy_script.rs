//! User-authored loop strategies in Rhai.
//!
//! A strategy script evaluates to a map describing an ordered list of steps — the
//! same shape as a built-in [`Strategy`], but written by the user:
//!
//! ```rhai
//! #{
//!     name: "my-loop",
//!     persist: false,           // reuse context across attempts? (monolithic = true)
//!     phases: [
//!         #{ system: "You plan...",  prompt: "Investigate {issue}\n{targets}", scope: "readonly" },
//!         #{ system: "You execute...", prompt: "Plan:\n{prev}\nApply it.",       scope: "full" },
//!     ],
//! }
//! ```
//!
//! A step may instead fan out: give it a `parallel` array of phases (all run
//! concurrently, read-only by contract) and an optional `join` (`"concat"` — the
//! default — or `"first"`) that says how the branch outputs feed the next step's
//! `{prev}`:
//!
//! ```rhai
//! #{ parallel: [ #{ system: "...", prompt: "angle A {issue}", scope: "readonly" },
//!                #{ system: "...", prompt: "angle B {issue}", scope: "readonly" } ],
//!    join: "concat" }
//! ```
//!
//! Prompt placeholders (`{issue}`, `{targets}`, `{prev}`, `{verdict}`) are
//! rendered by the engine at run time — see [`pirs_agent::strategy::render`].
//! Scripts are data-only: they build the step list and return it. There is no
//! access to the file system or tools from the script itself; the *phases* run
//! through the host's [`PhaseDriver`](pirs_agent::strategy::PhaseDriver).

use std::path::Path;

use anyhow::{anyhow, bail, Context as _};
use pirs_agent::strategy::{Join, Phase, Step, Strategy, ToolScope};
use rhai::{Array, Dynamic, Engine, Map};

/// Load a strategy from a Rhai script file. The strategy's default name is the
/// file stem (overridable by a `name:` field in the script).
pub fn load_strategy_file(path: &Path) -> anyhow::Result<Strategy> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("read strategy script {path:?}"))?;
    let default_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("script")
        .to_string();
    load_strategy_str(&src, &default_name).with_context(|| format!("in strategy script {path:?}"))
}

/// Load a strategy from Rhai source. `default_name` is used when the script omits
/// a `name:` field.
pub fn load_strategy_str(src: &str, default_name: &str) -> anyhow::Result<Strategy> {
    let engine = Engine::new();
    let value: Dynamic = engine
        .eval(src)
        .map_err(|e| anyhow!("strategy script failed to evaluate: {e}"))?;
    strategy_from_dynamic(value, default_name)
}

fn get_str(map: &Map, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.clone().into_string().ok())
}

/// Parse one phase map (`system`, `prompt`, `scope?`, `model?`). `where_` labels
/// the phase in error messages.
fn phase_from_map(pm: &Map, where_: &str) -> anyhow::Result<Phase> {
    let system =
        get_str(pm, "system").ok_or_else(|| anyhow!("{where_} is missing a `system` string"))?;
    let prompt =
        get_str(pm, "prompt").ok_or_else(|| anyhow!("{where_} is missing a `prompt` string"))?;
    let scope = match get_str(pm, "scope").as_deref().unwrap_or("full") {
        "readonly" | "read_only" | "read-only" => ToolScope::ReadOnly,
        "full" => ToolScope::Full,
        other => bail!("{where_} has unknown scope {other:?} (use \"readonly\" or \"full\")"),
    };
    // Optional per-phase model override — the Oracle lever.
    let model = get_str(pm, "model");
    Ok(Phase {
        system,
        prompt,
        scope,
        model,
    })
}

/// Parse one step: either a solo phase (`system`/`prompt` at the top level) or a
/// fan-out (`parallel: [...]` with an optional `join`).
fn step_from_map(sm: Map, i: usize) -> anyhow::Result<Step> {
    if let Some(par) = sm.get("parallel").cloned() {
        let arr = par
            .try_cast::<Array>()
            .ok_or_else(|| anyhow!("step {i}: `parallel` must be an array of phases"))?;
        if arr.is_empty() {
            bail!("step {i}: `parallel` must have at least one branch");
        }
        let mut branches = Vec::with_capacity(arr.len());
        for (b, item) in arr.into_iter().enumerate() {
            let pm = item
                .try_cast::<Map>()
                .ok_or_else(|| anyhow!("step {i} branch {b} must be a map"))?;
            branches.push(phase_from_map(&pm, &format!("step {i} branch {b}"))?);
        }
        let join = match get_str(&sm, "join").as_deref().unwrap_or("concat") {
            "concat" => Join::Concat,
            "first" => Join::First,
            other => bail!("step {i} has unknown join {other:?} (use \"concat\" or \"first\")"),
        };
        Ok(Step::Fan { branches, join })
    } else {
        Ok(Step::Solo(phase_from_map(&sm, &format!("phase {i}"))?))
    }
}

fn strategy_from_dynamic(value: Dynamic, default_name: &str) -> anyhow::Result<Strategy> {
    let map = value
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("script must return a map with a `phases` array"))?;
    strategy_from_map(map, default_name)
}

/// Build a [`Strategy`] from an already-cast Rhai map. Shared by the top-level
/// strategy loader and by profiles that carry an inline strategy.
pub fn strategy_from_map(map: Map, default_name: &str) -> anyhow::Result<Strategy> {
    let name = get_str(&map, "name").unwrap_or_else(|| default_name.to_string());
    let persist = map
        .get("persist")
        .and_then(|v| v.as_bool().ok())
        .unwrap_or(false);

    let phases_dyn = map
        .get("phases")
        .cloned()
        .ok_or_else(|| anyhow!("strategy is missing a `phases` array"))?;
    let phases_arr = phases_dyn
        .try_cast::<Array>()
        .ok_or_else(|| anyhow!("`phases` must be an array"))?;
    if phases_arr.is_empty() {
        bail!("strategy must have at least one phase");
    }

    let mut steps = Vec::with_capacity(phases_arr.len());
    for (i, item) in phases_arr.into_iter().enumerate() {
        let sm = item
            .try_cast::<Map>()
            .ok_or_else(|| anyhow!("step {i} must be a map"))?;
        steps.push(step_from_map(sm, i)?);
    }

    Ok(Strategy {
        name,
        steps,
        persist_across_attempts: persist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Extract the phase from a solo step, panicking if it is a fan-out.
    fn solo(step: &Step) -> &Phase {
        match step {
            Step::Solo(p) => p,
            Step::Fan { .. } => panic!("expected a solo step, got a fan-out"),
        }
    }

    #[test]
    fn loads_a_two_phase_plan_exec_strategy() {
        let src = r#"
            #{
                name: "custom-plan-exec",
                persist: false,
                phases: [
                    #{ system: "plan", prompt: "investigate {issue}", scope: "readonly" },
                    #{ system: "exec", prompt: "do {prev}", scope: "full" },
                ],
            }
        "#;
        let s = load_strategy_str(src, "fallback").unwrap();
        assert_eq!(s.name, "custom-plan-exec");
        assert!(!s.persist_across_attempts);
        assert_eq!(s.steps.len(), 2);
        assert_eq!(solo(&s.steps[0]).scope, ToolScope::ReadOnly);
        assert_eq!(solo(&s.steps[1]).scope, ToolScope::Full);
        assert!(solo(&s.steps[0]).prompt.contains("{issue}"));
    }

    #[test]
    fn scope_defaults_to_full_and_name_falls_back() {
        let src = r#"#{ phases: [ #{ system: "s", prompt: "p" } ] }"#;
        let s = load_strategy_str(src, "fallback").unwrap();
        assert_eq!(s.name, "fallback");
        assert_eq!(solo(&s.steps[0]).scope, ToolScope::Full);
        assert_eq!(solo(&s.steps[0]).model, None);
    }

    #[test]
    fn per_phase_model_defines_an_oracle() {
        let src = r#"
            #{ phases: [
                #{ system: "plan", prompt: "p", scope: "readonly" },
                #{ system: "critic", prompt: "c {prev}", scope: "readonly", model: "deepseek-v4-pro" },
                #{ system: "exec", prompt: "e {prev}", scope: "full" },
            ] }
        "#;
        let s = load_strategy_str(src, "x").unwrap();
        assert_eq!(solo(&s.steps[0]).model, None);
        assert_eq!(solo(&s.steps[1]).model, Some("deepseek-v4-pro".to_string()));
        assert_eq!(solo(&s.steps[2]).model, None);
    }

    #[test]
    fn a_script_can_build_phases_programmatically() {
        // The whole point of a *script*: phases need not be a literal. Here a loop
        // assembles a 3-round refine strategy.
        let src = r#"
            let phases = [];
            for i in 0..3 {
                phases.push(#{ system: "refine", prompt: "round " + i + " for {issue}", scope: "full" });
            }
            #{ name: "refine-3", phases: phases }
        "#;
        let s = load_strategy_str(src, "x").unwrap();
        assert_eq!(s.steps.len(), 3);
        assert!(solo(&s.steps[2]).prompt.contains("round 2"));
    }

    #[test]
    fn a_parallel_step_becomes_a_fan_out() {
        let src = r#"
            #{ name: "wide", phases: [
                #{ parallel: [
                    #{ system: "a", prompt: "angle A {issue}", scope: "readonly" },
                    #{ system: "b", prompt: "angle B {issue}", scope: "readonly" },
                ], join: "concat" },
                #{ system: "exec", prompt: "do {prev}", scope: "full" },
            ] }
        "#;
        let s = load_strategy_str(src, "x").unwrap();
        assert_eq!(s.steps.len(), 2);
        match &s.steps[0] {
            Step::Fan { branches, join } => {
                assert_eq!(branches.len(), 2);
                assert_eq!(*join, Join::Concat);
                assert_eq!(branches[0].scope, ToolScope::ReadOnly);
            }
            _ => panic!("first step must be a fan-out"),
        }
        assert_eq!(solo(&s.steps[1]).scope, ToolScope::Full);
    }

    #[test]
    fn join_defaults_to_concat_and_rejects_unknown() {
        let ok = r#"#{ phases: [ #{ parallel: [ #{ system: "a", prompt: "p", scope: "readonly" } ] } ] }"#;
        let s = load_strategy_str(ok, "x").unwrap();
        match &s.steps[0] {
            Step::Fan { join, .. } => assert_eq!(*join, Join::Concat),
            _ => panic!("expected fan-out"),
        }
        let bad =
            r#"#{ phases: [ #{ parallel: [ #{ system: "a", prompt: "p" } ], join: "vote" } ] }"#;
        let err = load_strategy_str(bad, "x").unwrap_err().to_string();
        assert!(err.contains("unknown join"), "{err}");
    }

    #[test]
    fn unknown_scope_is_a_clear_error() {
        let src = r#"#{ phases: [ #{ system: "s", prompt: "p", scope: "sandbox" } ] }"#;
        let err = load_strategy_str(src, "x").unwrap_err().to_string();
        assert!(err.contains("unknown scope"), "{err}");
    }

    #[test]
    fn missing_phases_is_rejected() {
        let err = load_strategy_str("#{ name: \"x\" }", "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("phases"), "{err}");
    }
}
