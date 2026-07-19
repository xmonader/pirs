//! Profiles — a composition root that bundles a [`Strategy`], a persona, a default
//! model, and a tool policy into one named "role" (a virtual employee).
//!
//! A [`Strategy`] says *how the work is structured* (which phases, what tools per
//! phase). A [`Profile`] wraps that with the rest of what makes a role: a persona
//! (a system-prompt preamble stamped onto every phase, e.g. "You are a security
//! reviewer"), a default model, and a [`ToolPolicy`] (an allow/deny filter over the
//! tool set the host offers). One manifest = one reusable role the user can pick by
//! name.
//!
//! The profile is applied by [`Profile::resolved_strategy`], which returns a new
//! [`Strategy`] with the persona prepended to each phase's system prompt and the
//! default model filled into any phase that didn't pin its own. Everything else —
//! running the phases, honouring the model override — is the existing engine, so a
//! profile needs no new execution machinery.

use crate::strategy::{Phase, Step, Strategy};

/// An allow/deny filter over tool names. `deny` always wins; `allow`, when set,
/// restricts to exactly its members (an empty `allow` means "no tools").
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolPolicy {
    /// When `Some`, only these tool names are permitted. `None` permits all
    /// (subject to `deny`).
    pub allow: Option<Vec<String>>,
    /// Tool names always removed, even if present in `allow`.
    pub deny: Vec<String>,
}

impl ToolPolicy {
    /// A policy that permits everything.
    pub fn allow_all() -> Self {
        Self::default()
    }

    /// Does this policy permit a tool by name? `deny` is checked first.
    pub fn permits(&self, name: &str) -> bool {
        if self.deny.iter().any(|d| d == name) {
            return false;
        }
        match &self.allow {
            Some(allow) => allow.iter().any(|a| a == name),
            None => true,
        }
    }

    /// Filter a slice of named items to those this policy permits, preserving order.
    pub fn filter<'a, T>(&self, items: &'a [T], name: impl Fn(&T) -> &str) -> Vec<&'a T> {
        items.iter().filter(|t| self.permits(name(t))).collect()
    }
}

/// A named role: a strategy plus the persona, model, and tool policy that turn it
/// into a virtual employee.
#[derive(Debug, Clone)]
pub struct Profile {
    pub name: String,
    /// Prepended to every phase's system prompt. `None` leaves prompts untouched.
    pub persona: Option<String>,
    /// Default model for phases that don't pin their own. `None` uses the run
    /// default.
    pub model: Option<String>,
    pub strategy: Strategy,
    pub tools: ToolPolicy,
}

impl Profile {
    /// A profile that just names a strategy, with no persona, model, or tool
    /// restriction — the identity wrapper.
    pub fn from_strategy(name: impl Into<String>, strategy: Strategy) -> Self {
        Profile {
            name: name.into(),
            persona: None,
            model: None,
            strategy,
            tools: ToolPolicy::allow_all(),
        }
    }

    /// The strategy with the profile baked in: persona prepended to each phase's
    /// system prompt, and the profile's default model filled into any phase that
    /// left its model unset. A phase that pinned its own model (the Oracle lever)
    /// keeps it — the profile default never overrides an explicit choice.
    pub fn resolved_strategy(&self) -> Strategy {
        let mut s = self.strategy.clone();
        for step in &mut s.steps {
            match step {
                Step::Solo(phase) => self.apply_to_phase(phase),
                Step::Fan { branches, .. } => {
                    for phase in branches {
                        self.apply_to_phase(phase);
                    }
                }
            }
        }
        s
    }

    fn apply_to_phase(&self, phase: &mut Phase) {
        if let Some(persona) = &self.persona {
            phase.system = format!("{persona}\n\n{}", phase.system);
        }
        if phase.model.is_none() {
            if let Some(model) = &self.model {
                phase.model = Some(model.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::strategy::ToolScope;

    #[test]
    fn deny_always_wins_over_allow() {
        let p = ToolPolicy {
            allow: Some(vec!["read".into(), "edit".into()]),
            deny: vec!["edit".into()],
        };
        assert!(p.permits("read"));
        assert!(!p.permits("edit")); // denied despite being allowed
        assert!(!p.permits("bash")); // not in allow
    }

    #[test]
    fn none_allow_permits_all_except_denied() {
        let p = ToolPolicy {
            allow: None,
            deny: vec!["bash".into()],
        };
        assert!(p.permits("read"));
        assert!(!p.permits("bash"));
    }

    #[test]
    fn filter_preserves_order_and_drops_denied() {
        let p = ToolPolicy {
            allow: None,
            deny: vec!["b".into()],
        };
        let names = ["a", "b", "c"];
        let kept = p.filter(&names, |s| s);
        assert_eq!(kept, vec![&"a", &"c"]);
    }

    #[test]
    fn persona_is_prepended_to_every_phase() {
        let profile = Profile {
            name: "security-reviewer".into(),
            persona: Some("You are a paranoid security reviewer.".into()),
            model: None,
            strategy: Strategy::plan_exec(),
            tools: ToolPolicy::allow_all(),
        };
        let resolved = profile.resolved_strategy();
        for step in &resolved.steps {
            if let Step::Solo(p) = step {
                assert!(
                    p.system
                        .starts_with("You are a paranoid security reviewer."),
                    "persona missing from: {}",
                    p.system
                );
            }
        }
    }

    #[test]
    fn default_model_fills_unpinned_phases_only() {
        // plan-oracle-exec pins the critic to a specific model; the profile default
        // must fill the other phases without clobbering the critic's pin.
        let profile = Profile {
            name: "cheap-with-oracle".into(),
            persona: None,
            model: Some("cheap-model".into()),
            strategy: Strategy::plan_oracle_exec("strong-oracle"),
            tools: ToolPolicy::allow_all(),
        };
        let resolved = profile.resolved_strategy();
        let models: Vec<Option<String>> = resolved
            .steps
            .iter()
            .map(|s| match s {
                Step::Solo(p) => p.model.clone(),
                Step::Fan { .. } => None,
            })
            .collect();
        assert_eq!(models[0], Some("cheap-model".into())); // plan: filled
        assert_eq!(models[1], Some("strong-oracle".into())); // critic: kept its pin
        assert_eq!(models[2], Some("cheap-model".into())); // exec: filled
    }

    #[test]
    fn persona_applies_to_fan_out_branches() {
        let profile = Profile {
            name: "wide".into(),
            persona: Some("PERSONA".into()),
            model: Some("m".into()),
            strategy: Strategy::wide_plan_exec(3),
            tools: ToolPolicy::allow_all(),
        };
        let resolved = profile.resolved_strategy();
        match &resolved.steps[0] {
            Step::Fan { branches, .. } => {
                assert_eq!(branches.len(), 3);
                for b in branches {
                    assert!(b.system.starts_with("PERSONA"));
                    assert_eq!(b.model, Some("m".into()));
                    assert_eq!(b.scope, ToolScope::ReadOnly);
                }
            }
            Step::Solo(_) => panic!("expected fan-out"),
        }
    }

    #[test]
    fn from_strategy_is_the_identity_wrapper() {
        let s = Strategy::monolithic();
        let p = Profile::from_strategy("plain", s.clone());
        let resolved = p.resolved_strategy();
        // No persona, no model → phases unchanged.
        if let (Step::Solo(a), Step::Solo(b)) = (&s.steps[0], &resolved.steps[0]) {
            assert_eq!(a.system, b.system);
            assert_eq!(b.model, None);
        } else {
            panic!("expected solo steps");
        }
        assert!(p.tools.permits("anything"));
    }
}
