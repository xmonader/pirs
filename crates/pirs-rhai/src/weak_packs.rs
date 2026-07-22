//! Bundled extension catalog + profile-driven loading.
//!
//! Packs live as embedded sources (`extensions/*.rhai`). Which ones load for a
//! session is decided by the active **profile** (`packs` field):
//!
//! - built-in `default` (implicit) → `packs: "*"` (full catalog)
//! - explicit `--profile <name|path>` → that script's `packs`
//! - user override: `~/.pirs/profiles/default.rhai` / `.pirs/profiles/default.rhai`
//!
//! CLI `--weak` does not change packs. Project/user extension dirs still load
//! *after* the profile pack set and can override tools by name (last wins).

/// Pack stems in full-catalog order (used when a profile says `packs: "*"`).
pub const BUNDLED_ORDER: &[&str] = &[
    "weak-model",
    "context-janitor",
    "env-doctor",
    "goal",
    "audit-log",
    "auto-checkpoint",
    "blame",
    "blast-radius-judge",
    "browser-cdp-workflow",
    "btw",
    "chapter-spine",
    "conductor",
    "cost-sentinel",
    "critic-arena",
    "critic",
    "diff-shield",
    "dirty-guard",
    "dmail",
    "file-checkpoints",
    "fork",
    "guardrails",
    "hive-note",
    "instincts",
    "mutation-guard",
    "path-guard",
    "project-discipline",
    "relay-race",
    "repo-pulse",
    "review-gate",
    "runs",
    "safe-edit",
    "sandbox",
    "semantic-bookmarks",
    "session-discipline",
    "session-handoff",
    "shadow-verify",
    "skill-crystallizer",
    "spec-check",
    "spend-caps",
    "stash-checkpoint",
    "strict-plan",
    "subagents",
    "swarm",
    "telemetry",
    "verify-guard",
    "verify-impact",
    "web-tools",
    "word-count",
    "workflow"
];

/// `(display_name, source)` for every catalog pack. Order matches [`BUNDLED_ORDER`].
pub const BUNDLED: &[(&str, &str)] = &[
    ("bundled:weak-model.rhai", include_str!("../../../extensions/weak-model.rhai")),
    ("bundled:context-janitor.rhai", include_str!("../../../extensions/context-janitor.rhai")),
    ("bundled:env-doctor.rhai", include_str!("../../../extensions/env-doctor.rhai")),
    ("bundled:goal.rhai", include_str!("../../../extensions/goal.rhai")),
    ("bundled:audit-log.rhai", include_str!("../../../extensions/audit-log.rhai")),
    ("bundled:auto-checkpoint.rhai", include_str!("../../../extensions/auto-checkpoint.rhai")),
    ("bundled:blame.rhai", include_str!("../../../extensions/blame.rhai")),
    ("bundled:blast-radius-judge.rhai", include_str!("../../../extensions/blast-radius-judge.rhai")),
    ("bundled:browser-cdp-workflow.rhai", include_str!("../../../extensions/browser-cdp-workflow.rhai")),
    ("bundled:btw.rhai", include_str!("../../../extensions/btw.rhai")),
    ("bundled:chapter-spine.rhai", include_str!("../../../extensions/chapter-spine.rhai")),
    ("bundled:conductor.rhai", include_str!("../../../extensions/conductor.rhai")),
    ("bundled:cost-sentinel.rhai", include_str!("../../../extensions/cost-sentinel.rhai")),
    ("bundled:critic-arena.rhai", include_str!("../../../extensions/critic-arena.rhai")),
    ("bundled:critic.rhai", include_str!("../../../extensions/critic.rhai")),
    ("bundled:diff-shield.rhai", include_str!("../../../extensions/diff-shield.rhai")),
    ("bundled:dirty-guard.rhai", include_str!("../../../extensions/dirty-guard.rhai")),
    ("bundled:dmail.rhai", include_str!("../../../extensions/dmail.rhai")),
    ("bundled:file-checkpoints.rhai", include_str!("../../../extensions/file-checkpoints.rhai")),
    ("bundled:fork.rhai", include_str!("../../../extensions/fork.rhai")),
    ("bundled:guardrails.rhai", include_str!("../../../extensions/guardrails.rhai")),
    ("bundled:hive-note.rhai", include_str!("../../../extensions/hive-note.rhai")),
    ("bundled:instincts.rhai", include_str!("../../../extensions/instincts.rhai")),
    ("bundled:mutation-guard.rhai", include_str!("../../../extensions/mutation-guard.rhai")),
    ("bundled:path-guard.rhai", include_str!("../../../extensions/path-guard.rhai")),
    ("bundled:project-discipline.rhai", include_str!("../../../extensions/project-discipline.rhai")),
    ("bundled:relay-race.rhai", include_str!("../../../extensions/relay-race.rhai")),
    ("bundled:repo-pulse.rhai", include_str!("../../../extensions/repo-pulse.rhai")),
    ("bundled:review-gate.rhai", include_str!("../../../extensions/review-gate.rhai")),
    ("bundled:runs.rhai", include_str!("../../../extensions/runs.rhai")),
    ("bundled:safe-edit.rhai", include_str!("../../../extensions/safe-edit.rhai")),
    ("bundled:sandbox.rhai", include_str!("../../../extensions/sandbox.rhai")),
    ("bundled:semantic-bookmarks.rhai", include_str!("../../../extensions/semantic-bookmarks.rhai")),
    ("bundled:session-discipline.rhai", include_str!("../../../extensions/session-discipline.rhai")),
    ("bundled:session-handoff.rhai", include_str!("../../../extensions/session-handoff.rhai")),
    ("bundled:shadow-verify.rhai", include_str!("../../../extensions/shadow-verify.rhai")),
    ("bundled:skill-crystallizer.rhai", include_str!("../../../extensions/skill-crystallizer.rhai")),
    ("bundled:spec-check.rhai", include_str!("../../../extensions/spec-check.rhai")),
    ("bundled:spend-caps.rhai", include_str!("../../../extensions/spend-caps.rhai")),
    ("bundled:stash-checkpoint.rhai", include_str!("../../../extensions/stash-checkpoint.rhai")),
    ("bundled:strict-plan.rhai", include_str!("../../../extensions/strict-plan.rhai")),
    ("bundled:subagents.rhai", include_str!("../../../extensions/subagents.rhai")),
    ("bundled:swarm.rhai", include_str!("../../../extensions/swarm.rhai")),
    ("bundled:telemetry.rhai", include_str!("../../../extensions/telemetry.rhai")),
    ("bundled:verify-guard.rhai", include_str!("../../../extensions/verify-guard.rhai")),
    ("bundled:verify-impact.rhai", include_str!("../../../extensions/verify-impact.rhai")),
    ("bundled:web-tools.rhai", include_str!("../../../extensions/web-tools.rhai")),
    ("bundled:word-count.rhai", include_str!("../../../extensions/word-count.rhai")),
    ("bundled:workflow.rhai", include_str!("../../../extensions/workflow.rhai"))
];

/// Subset used by the built-in `weak` profile.
pub const WEAK_ORDER: &[&str] = &[
    "weak-model",
    "context-janitor",
    "env-doctor",
    "goal",
];

/// Built-in profile source for `--profile default` / implicit session packs.
pub const DEFAULT_PROFILE: &str = include_str!("../builtins/default.profile.rhai");

/// Built-in profile source for `--profile weak`.
pub const WEAK_PROFILE: &str = include_str!("../builtins/weak.profile.rhai");

/// Expand a profile's `packs` field into concrete catalog stems.
///
/// - empty → no catalog packs
/// - any entry `"*"` or `"all"` → full [`BUNDLED_ORDER`]
/// - otherwise → the listed stems (unknown stems error at load time)
pub fn expand_packs(spec: &[String]) -> Vec<String> {
    if spec.is_empty() {
        return Vec::new();
    }
    if spec.iter().any(|s| {
        let t = s.trim();
        t == "*" || t.eq_ignore_ascii_case("all")
    }) {
        return BUNDLED_ORDER.iter().map(|s| (*s).to_string()).collect();
    }
    spec.iter()
        .map(|s| s.trim().trim_end_matches(".rhai").to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Look up embedded source for a pack stem (`"goal"` → bundled goal.rhai).
pub fn bundled_source(stem: &str) -> Option<(&'static str, &'static str)> {
    let stem = stem.trim().trim_end_matches(".rhai");
    BUNDLED
        .iter()
        .find(|(name, _)| {
            name.trim_start_matches("bundled:")
                .trim_end_matches(".rhai")
                == stem
        })
        .map(|(n, s)| (*n, *s))
}

/// Load the given pack stems into `host` (profile order). Errors go to
/// `host.load_errors` so one broken pack does not abort the session.
pub fn load_stems(host: &mut crate::ExtensionHost, stems: &[String]) {
    for stem in stems {
        match bundled_source(stem) {
            Some((name, src)) => {
                if let Err(e) = host.load_source(src, name.to_string()) {
                    host.load_errors
                        .push(format!("{name}: failed to load bundled pack: {e:#}"));
                }
            }
            None => host.load_errors.push(format!(
                "bundled:{stem}: unknown pack stem (not in catalog)"
            )),
        }
    }
}

/// Load packs from a profile's `packs` field (expands `"*"`).
pub fn load_profile_packs(host: &mut crate::ExtensionHost, packs: &[String]) {
    let stems = expand_packs(packs);
    load_stems(host, &stems);
}

/// Load the full catalog (same as profile `packs: "*"`).
pub fn load_into(host: &mut crate::ExtensionHost) {
    load_profile_packs(host, &["*".into()]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expand_star_is_full_order() {
        let got = expand_packs(&["*".into()]);
        assert_eq!(got.len(), BUNDLED_ORDER.len());
        assert_eq!(got[0], "weak-model");
        assert_eq!(got[3], "goal");
    }

    #[test]
    fn expand_list_preserves_order() {
        let got = expand_packs(&["goal".into(), "btw".into()]);
        assert_eq!(got, vec!["goal".to_string(), "btw".to_string()]);
    }

    #[test]
    fn all_bundled_packs_load() {
        let mut host = crate::ExtensionHost::new();
        load_into(&mut host);
        assert!(
            host.load_errors.is_empty(),
            "bundled packs must load cleanly: {:?}",
            host.load_errors
        );
        assert_eq!(host.extension_names().len(), BUNDLED.len());
        let cmds: Vec<_> = host.commands().into_iter().map(|(n, _)| n).collect();
        assert!(cmds.iter().any(|n| n == "goal"), "goal cmd missing: {cmds:?}");
        assert!(cmds.iter().any(|n| n == "btw"), "btw cmd missing: {cmds:?}");
    }

    #[test]
    fn load_stems_subset() {
        let mut host = crate::ExtensionHost::new();
        load_stems(&mut host, &["goal".into(), "btw".into()]);
        assert!(host.load_errors.is_empty(), "{:?}", host.load_errors);
        assert_eq!(host.extension_names().len(), 2);
        let cmds: Vec<_> = host.commands().into_iter().map(|(n, _)| n).collect();
        assert!(cmds.contains(&"goal".into()));
        assert!(cmds.contains(&"btw".into()));
    }

    #[test]
    fn bundled_order_matches_sources_and_is_deterministic() {
        assert_eq!(BUNDLED.len(), BUNDLED_ORDER.len());
        for (i, stem) in BUNDLED_ORDER.iter().enumerate() {
            assert!(
                BUNDLED[i].0.contains(stem),
                "BUNDLED[{i}] display name {:?} must contain stem {stem}",
                BUNDLED[i].0
            );
        }
        assert_eq!(&BUNDLED_ORDER[..4], WEAK_ORDER);
    }

    #[test]
    fn default_and_weak_profiles_parse() {
        let d = crate::profile_script::load_profile_str(DEFAULT_PROFILE, "default").unwrap();
        assert_eq!(d.name, "default");
        assert_eq!(d.packs, vec!["*".to_string()]);
        assert_eq!(d.strategy.name, "monolithic");

        let w = crate::profile_script::load_profile_str(WEAK_PROFILE, "weak").unwrap();
        assert_eq!(w.name, "weak");
        assert_eq!(w.strategy.name, "plan-exec");
        assert!(w.persona.is_some());
        assert_eq!(
            w.packs,
            vec![
                "weak-model".to_string(),
                "context-janitor".to_string(),
                "env-doctor".to_string(),
                "goal".to_string()
            ]
        );
        let stems = expand_packs(&w.packs);
        assert_eq!(stems.len(), 4);
    }
}
