//! Strong-model orchestration primitives (planner + steerer), A/B-gated.
//!
//! A stronger model can *plan* (pick and order which candidate sites to try) and
//! *steer* (emit advisory hints between attempts). The whole design rests on one
//! rule from the plan: **hints are advisory; invariants are code.** So the model
//! never gets to assert anything the harness then trusts. Concretely:
//!
//!  - [`plan_next`] can only **reorder and filter the candidate set** produced by
//!    localization. A file the model names that isn't a real candidate is dropped.
//!    It cannot invent an edit site, and it cannot declare a task solved — only
//!    the [`gate`](crate::gate) does that, over real red→green flips.
//!  - Every path degrades safe: a model error, a timeout, or unparseable output
//!    falls back to the deterministic candidate order. Turning orchestration on
//!    can cost latency; it can never change a correctness outcome.
//!
//! That is what makes the A/B flag honest — with the oracle disabled the driver
//! is byte-for-byte deterministic, so any measured delta is the model's doing.

use std::path::PathBuf;

use crate::localize::Candidate;

/// The `ask_model` primitive. Implementors call whatever model/endpoint; the
/// harness only ever consumes the returned text, and only through the validated
/// helpers below.
pub trait ModelOracle {
    /// Ask the model. Returning `Err` is fine — callers degrade to the
    /// deterministic path, so an oracle need not paper over its own failures.
    fn ask(&self, system: &str, user: &str) -> anyhow::Result<String>;
}

/// A validated planner decision: a reordered/filtered subset of the candidate
/// files, plus an explicit give-up. This is the *only* shape a plan can take;
/// any other field the model emits is ignored.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanDecision {
    /// Candidate files to try, in the model's preferred order. Always a subset
    /// of the input candidates (validated), never empty unless `give_up`.
    pub ordered_files: Vec<PathBuf>,
    /// The model judged no candidate worth trying. Advisory: the caller may still
    /// exhaust attempts; this only lets a strong model cut a hopeless search short.
    pub give_up: bool,
}

/// Ask the oracle to order the candidate sites, then **validate hard**: keep only
/// files that are real candidates, dedup, and append any candidate the model
/// omitted (so filtering never silently drops a site the deterministic path would
/// have tried). On any failure — oracle error, no JSON, malformed — fall back to
/// the deterministic candidate order with `give_up = false`.
pub fn plan_next(
    oracle: &dyn ModelOracle,
    candidates: &[Candidate],
    last_failure: Option<&str>,
) -> PlanDecision {
    let fallback = || PlanDecision {
        ordered_files: candidates.iter().map(|c| c.file.clone()).collect(),
        give_up: false,
    };

    let system = "You order candidate edit sites for a bug fix. Reply with ONLY a JSON \
                  object: {\"files\": [\"path\", ...], \"give_up\": false}. Use only paths \
                  from the provided list. Do not invent paths.";
    let mut user = String::from("Candidate files (most-likely first):\n");
    for c in candidates {
        user.push_str(&format!("- {} (score {:.3})\n", c.file.display(), c.score));
    }
    if let Some(f) = last_failure {
        user.push_str(&format!("\nPrevious attempt outcome: {f}\n"));
    }

    let raw = match oracle.ask(system, &user) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!("planner oracle failed, using deterministic order: {e}");
            return fallback();
        }
    };
    let Some(json) = extract_json_object(&raw) else {
        tracing::warn!("planner returned no JSON object, using deterministic order");
        return fallback();
    };
    let parsed: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            tracing::warn!("planner JSON invalid ({e}), using deterministic order");
            return fallback();
        }
    };

    validate_plan(&parsed, candidates)
}

/// Enforce the invariant: the plan is a permutation-with-filter of the real
/// candidate set. Unknown paths are dropped; omitted candidates are re-appended
/// in their original order so nothing the deterministic path would try is lost.
fn validate_plan(parsed: &serde_json::Value, candidates: &[Candidate]) -> PlanDecision {
    use std::collections::HashSet;

    let valid: HashSet<PathBuf> = candidates.iter().map(|c| c.file.clone()).collect();
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut ordered: Vec<PathBuf> = Vec::new();

    if let Some(arr) = parsed.get("files").and_then(|v| v.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                let p = PathBuf::from(s);
                if valid.contains(&p) && seen.insert(p.clone()) {
                    ordered.push(p);
                }
            }
        }
    }
    // Re-append any candidate the model dropped, preserving deterministic order.
    for c in candidates {
        if seen.insert(c.file.clone()) {
            ordered.push(c.file.clone());
        }
    }

    let give_up = parsed.get("give_up").and_then(|v| v.as_bool()).unwrap_or(false);
    PlanDecision { ordered_files: ordered, give_up }
}

/// An advisory steering hint to fold into the executor's context before its next
/// attempt. It is plain text — it carries no authority, only suggestion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Hint(pub String);

/// Ask the oracle for a short hint given the situation so far. Returns `None` on
/// any failure or an empty response — the executor simply proceeds without one.
pub fn steer_hint(oracle: &dyn ModelOracle, situation: &str) -> Option<Hint> {
    let system = "You give ONE short, concrete hint (max 2 sentences) to help fix a failing \
                  test. No preamble. If you have nothing useful, reply with an empty line.";
    match oracle.ask(system, situation) {
        Ok(s) => {
            let t = s.trim();
            if t.is_empty() {
                None
            } else {
                Some(Hint(t.to_string()))
            }
        }
        Err(e) => {
            tracing::warn!("steerer oracle failed, proceeding without a hint: {e}");
            None
        }
    }
}

/// Extract the first balanced top-level `{...}` object from arbitrary text, so a
/// model that wraps JSON in prose or code fences still parses. Ignores braces
/// inside double-quoted strings (with escape handling).
fn extract_json_object(text: &str) -> Option<String> {
    let bytes = text.as_bytes();
    let start = text.find('{')?;
    let mut depth = 0usize;
    let mut in_str = false;
    let mut escaped = false;
    for i in start..bytes.len() {
        let c = bytes[i] as char;
        if in_str {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == '"' {
                in_str = false;
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..=i].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    struct FixedOracle(&'static str);
    impl ModelOracle for FixedOracle {
        fn ask(&self, _s: &str, _u: &str) -> anyhow::Result<String> {
            Ok(self.0.to_string())
        }
    }
    struct FailingOracle;
    impl ModelOracle for FailingOracle {
        fn ask(&self, _s: &str, _u: &str) -> anyhow::Result<String> {
            anyhow::bail!("network down")
        }
    }

    fn cands(files: &[&str]) -> Vec<Candidate> {
        files
            .iter()
            .enumerate()
            .map(|(i, f)| Candidate {
                file: PathBuf::from(f),
                symbol: None,
                score: 1.0 - i as f64 * 0.1,
            })
            .collect()
    }

    #[test]
    fn valid_plan_reorders_within_candidate_set() {
        let c = cands(&["a.py", "b.py", "c.py"]);
        let oracle = FixedOracle(r#"{"files": ["c.py", "a.py"], "give_up": false}"#);
        let plan = plan_next(&oracle, &c, None);
        // Model's order first, then the omitted candidate (b) re-appended.
        assert_eq!(
            plan.ordered_files,
            vec![PathBuf::from("c.py"), PathBuf::from("a.py"), PathBuf::from("b.py")]
        );
        assert!(!plan.give_up);
    }

    #[test]
    fn invented_paths_are_dropped() {
        let c = cands(&["a.py", "b.py"]);
        // Model tries to smuggle in a path that is not a candidate.
        let oracle = FixedOracle(r#"{"files": ["/etc/passwd", "b.py"], "give_up": false}"#);
        let plan = plan_next(&oracle, &c, None);
        assert_eq!(plan.ordered_files, vec![PathBuf::from("b.py"), PathBuf::from("a.py")]);
        assert!(!plan.ordered_files.iter().any(|p| p.to_str() == Some("/etc/passwd")));
    }

    #[test]
    fn garbage_output_falls_back_to_deterministic_order() {
        let c = cands(&["a.py", "b.py"]);
        let oracle = FixedOracle("I think you should look at the parser, honestly.");
        let plan = plan_next(&oracle, &c, None);
        assert_eq!(plan.ordered_files, vec![PathBuf::from("a.py"), PathBuf::from("b.py")]);
        assert!(!plan.give_up);
    }

    #[test]
    fn oracle_error_degrades_safe() {
        let c = cands(&["a.py", "b.py"]);
        let plan = plan_next(&FailingOracle, &c, Some("FixNoFlip"));
        assert_eq!(plan.ordered_files, vec![PathBuf::from("a.py"), PathBuf::from("b.py")]);
    }

    #[test]
    fn give_up_is_honored() {
        let c = cands(&["a.py"]);
        let oracle = FixedOracle(r#"prose... {"files": [], "give_up": true} ...more"#);
        let plan = plan_next(&oracle, &c, None);
        assert!(plan.give_up);
        // Even on give_up, candidates are preserved for a caller that ignores it.
        assert_eq!(plan.ordered_files, vec![PathBuf::from("a.py")]);
    }

    #[test]
    fn json_extracted_from_fenced_prose() {
        let obj = extract_json_object("```json\n{\"a\": 1, \"b\": {\"c\": 2}}\n```").unwrap();
        assert_eq!(obj, r#"{"a": 1, "b": {"c": 2}}"#);
    }

    #[test]
    fn json_extraction_ignores_braces_in_strings() {
        let obj = extract_json_object(r#"{"msg": "a } brace", "n": 1}"#).unwrap();
        assert_eq!(obj, r#"{"msg": "a } brace", "n": 1}"#);
    }

    #[test]
    fn steer_hint_returns_text_or_none() {
        let oracle = FixedOracle("  Check the off-by-one in the loop bound.  ");
        assert_eq!(
            steer_hint(&oracle, "situation"),
            Some(Hint("Check the off-by-one in the loop bound.".to_string()))
        );
        let empty = FixedOracle("   ");
        assert_eq!(steer_hint(&empty, "situation"), None);
        assert_eq!(steer_hint(&FailingOracle, "situation"), None);
    }
}
