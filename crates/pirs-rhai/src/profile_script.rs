//! User-authored profiles (roles) in Rhai.
//!
//! A profile manifest bundles a strategy with the persona, model, and tool policy
//! that make it a role. The `strategy` field is either the name of a built-in
//! (`"plan-exec"`, `"wide-plan-exec"`, …) or an inline strategy map in the same
//! shape [`strategy_script`](crate::strategy_script) accepts:
//!
//! ```rhai
//! #{
//!     name: "security-reviewer",
//!     persona: "You are a paranoid security reviewer. Assume every input is hostile.",
//!     model: "deepseek-v4-pro",
//!     strategy: "plan-critic-exec",          // a built-in by name…
//!     tools: #{ deny: ["bash"] },            // …minus the shell
//! }
//! ```
//!
//! or with an inline strategy:
//!
//! ```rhai
//! #{
//!     name: "quick-fixer",
//!     strategy: #{ phases: [ #{ system: "Fix it.", prompt: "{issue}", scope: "full" } ] },
//!     tools: #{ allow: ["read", "edit", "run_tests"] },
//! }
//! ```

use std::path::Path;

use anyhow::{anyhow, Context as _};
use pirs_agent::profile::{Profile, ToolPolicy};
use pirs_agent::strategy::Strategy;
use rhai::{Array, Dynamic, Engine, Map};

use crate::strategy_script::strategy_from_map;

/// Load a profile from a Rhai script file. The default name is the file stem.
pub fn load_profile_file(path: &Path) -> anyhow::Result<Profile> {
    let src =
        std::fs::read_to_string(path).with_context(|| format!("read profile script {path:?}"))?;
    let default_name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("profile")
        .to_string();
    load_profile_str(&src, &default_name).with_context(|| format!("in profile script {path:?}"))
}

/// Load a profile from Rhai source. `default_name` is used when the script omits a
/// `name:` field.
pub fn load_profile_str(src: &str, default_name: &str) -> anyhow::Result<Profile> {
    let engine = Engine::new();
    let value: Dynamic = engine
        .eval(src)
        .map_err(|e| anyhow!("profile script failed to evaluate: {e}"))?;
    let map = value
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("profile script must return a map"))?;
    profile_from_map(map, default_name)
}

fn get_str(map: &Map, key: &str) -> Option<String> {
    map.get(key).and_then(|v| v.clone().into_string().ok())
}

/// Parse the `strategy` field: a string names a built-in, a map is inline.
fn strategy_field(map: &Map, default_name: &str) -> anyhow::Result<Strategy> {
    let dyn_val = map
        .get("strategy")
        .cloned()
        .ok_or_else(|| anyhow!("profile is missing a `strategy` (a built-in name or a map)"))?;

    if let Some(name) = dyn_val.clone().into_string().ok().filter(|s| !s.is_empty()) {
        return Strategy::builtin(&name)
            .ok_or_else(|| anyhow!("unknown built-in strategy {name:?}"));
    }
    let sm = dyn_val
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("`strategy` must be a built-in name (string) or a strategy map"))?;
    strategy_from_map(sm, default_name)
}

/// Parse the optional `tools` policy: `#{ allow: [...], deny: [...] }`.
fn tool_policy(map: &Map) -> anyhow::Result<ToolPolicy> {
    let Some(tools_dyn) = map.get("tools").cloned() else {
        return Ok(ToolPolicy::allow_all());
    };
    let tm = tools_dyn
        .try_cast::<Map>()
        .ok_or_else(|| anyhow!("`tools` must be a map with `allow`/`deny` arrays"))?;
    let list = |key: &str| -> anyhow::Result<Option<Vec<String>>> {
        match tm.get(key).cloned() {
            None => Ok(None),
            Some(v) => {
                let arr = v
                    .try_cast::<Array>()
                    .ok_or_else(|| anyhow!("`tools.{key}` must be an array of tool names"))?;
                let names = arr
                    .into_iter()
                    .map(|d| {
                        d.into_string()
                            .map_err(|_| anyhow!("`tools.{key}` entries must be strings"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                Ok(Some(names))
            }
        }
    };
    Ok(ToolPolicy {
        allow: list("allow")?,
        deny: list("deny")?.unwrap_or_default(),
    })
}

fn profile_from_map(map: Map, default_name: &str) -> anyhow::Result<Profile> {
    let name = get_str(&map, "name").unwrap_or_else(|| default_name.to_string());
    let persona = get_str(&map, "persona");
    let model = get_str(&map, "model");
    let strategy = strategy_field(&map, &name)?;
    let tools = tool_policy(&map)?;
    Ok(Profile {
        name,
        persona,
        model,
        strategy,
        tools,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_agent::strategy::Step;

    #[test]
    fn loads_a_profile_referencing_a_builtin_strategy() {
        let src = r#"
            #{
                name: "security-reviewer",
                persona: "You are a paranoid security reviewer.",
                model: "deepseek-v4-pro",
                strategy: "plan-critic-exec",
                tools: #{ deny: ["bash"] },
            }
        "#;
        let p = load_profile_str(src, "fallback").unwrap();
        assert_eq!(p.name, "security-reviewer");
        assert_eq!(
            p.persona.as_deref(),
            Some("You are a paranoid security reviewer.")
        );
        assert_eq!(p.model.as_deref(), Some("deepseek-v4-pro"));
        assert_eq!(p.strategy.name, "plan-critic-exec");
        assert_eq!(p.strategy.steps.len(), 3);
        assert!(!p.tools.permits("bash"));
        assert!(p.tools.permits("read"));
    }

    #[test]
    fn loads_a_profile_with_an_inline_strategy() {
        let src = r#"
            #{
                name: "quick-fixer",
                strategy: #{ phases: [ #{ system: "Fix it.", prompt: "{issue}", scope: "full" } ] },
                tools: #{ allow: ["read", "edit", "run_tests"] },
            }
        "#;
        let p = load_profile_str(src, "fallback").unwrap();
        assert_eq!(p.strategy.steps.len(), 1);
        assert!(matches!(p.strategy.steps[0], Step::Solo(_)));
        assert!(p.tools.permits("edit"));
        assert!(!p.tools.permits("bash")); // allow-list excludes it
    }

    #[test]
    fn resolved_strategy_bakes_persona_and_model() {
        let src = r#"
            #{ name: "r", persona: "ROLE", model: "m", strategy: "plan-exec" }
        "#;
        let p = load_profile_str(src, "x").unwrap();
        let resolved = p.resolved_strategy();
        if let Step::Solo(phase) = &resolved.steps[0] {
            assert!(phase.system.starts_with("ROLE"));
            assert_eq!(phase.model.as_deref(), Some("m"));
        } else {
            panic!("expected solo");
        }
    }

    #[test]
    fn missing_strategy_is_rejected() {
        let err = load_profile_str(r#"#{ name: "x" }"#, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("strategy"), "{err}");
    }

    #[test]
    fn unknown_builtin_strategy_is_a_clear_error() {
        let err = load_profile_str(r#"#{ strategy: "nope" }"#, "x")
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown built-in strategy"), "{err}");
    }

    #[test]
    fn no_tools_field_permits_everything() {
        let p = load_profile_str(r#"#{ strategy: "monolithic" }"#, "x").unwrap();
        assert!(p.tools.permits("bash"));
        assert!(p.tools.permits("edit"));
    }
}
