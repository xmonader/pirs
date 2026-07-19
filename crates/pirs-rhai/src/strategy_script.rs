//! User-authored loop strategies in Rhai.
//!
//! A strategy script evaluates to a map describing an ordered phase list — the
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
//! Prompt placeholders (`{issue}`, `{targets}`, `{prev}`, `{verdict}`) are
//! rendered by the engine at run time — see [`pirs_agent::strategy::render`].
//! Scripts are data-only: they build the phase list and return it. There is no
//! access to the file system or tools from the script itself; the *phases* run
//! through the host's [`PhaseDriver`](pirs_agent::strategy::PhaseDriver).

use std::path::Path;

use anyhow::{anyhow, bail, Context as _};
use pirs_agent::strategy::{Phase, Strategy, ToolScope};
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

fn strategy_from_dynamic(value: Dynamic, default_name: &str) -> anyhow::Result<Strategy> {
    let map = value
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("script must return a map with a `phases` array"))?;

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

    let mut phases = Vec::with_capacity(phases_arr.len());
    for (i, item) in phases_arr.into_iter().enumerate() {
        let pm = item
            .try_cast::<Map>()
            .ok_or_else(|| anyhow!("phase {i} must be a map"))?;
        let system = get_str(&pm, "system")
            .ok_or_else(|| anyhow!("phase {i} is missing a `system` string"))?;
        let prompt = get_str(&pm, "prompt")
            .ok_or_else(|| anyhow!("phase {i} is missing a `prompt` string"))?;
        let scope = match get_str(&pm, "scope").as_deref().unwrap_or("full") {
            "readonly" | "read_only" | "read-only" => ToolScope::ReadOnly,
            "full" => ToolScope::Full,
            other => bail!("phase {i} has unknown scope {other:?} (use \"readonly\" or \"full\")"),
        };
        // Optional per-phase model override — the Oracle lever.
        let model = get_str(&pm, "model");
        phases.push(Phase {
            system,
            prompt,
            scope,
            model,
        });
    }

    Ok(Strategy {
        name,
        phases,
        persist_across_attempts: persist,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(s.phases.len(), 2);
        assert_eq!(s.phases[0].scope, ToolScope::ReadOnly);
        assert_eq!(s.phases[1].scope, ToolScope::Full);
        assert!(s.phases[0].prompt.contains("{issue}"));
    }

    #[test]
    fn scope_defaults_to_full_and_name_falls_back() {
        let src = r#"#{ phases: [ #{ system: "s", prompt: "p" } ] }"#;
        let s = load_strategy_str(src, "fallback").unwrap();
        assert_eq!(s.name, "fallback");
        assert_eq!(s.phases[0].scope, ToolScope::Full);
        assert_eq!(s.phases[0].model, None);
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
        assert_eq!(s.phases[0].model, None);
        assert_eq!(s.phases[1].model, Some("deepseek-v4-pro".to_string()));
        assert_eq!(s.phases[2].model, None);
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
        assert_eq!(s.phases.len(), 3);
        assert!(s.phases[2].prompt.contains("round 2"));
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
