//! Probe-confirm — the "verify before trust" step for runner discovery. A
//! candidate [`RunnerSpec`] is only trusted once its `list_cmd` (collect-only /
//! dry-run) runs cleanly and enumerates at least one test. A failing probe's
//! stderr is *kept*, not discarded: it is usually the environment-repair signal
//! (a missing dependency, an unbuilt extension).

use std::path::Path;

use crate::proc::run_capture;
use crate::types::RunnerSpec;

#[derive(Debug, Clone)]
pub struct ProbeResult {
    /// The runner ran and enumerated ≥1 test — safe to trust.
    pub confirmed: bool,
    /// Best-effort count of enumerated tests (non-empty stdout lines).
    pub listed: usize,
    /// Surfaced on failure as the environment-repair signal.
    pub stderr: String,
    pub timed_out: bool,
}

/// Run the spec's `list_cmd` and decide whether the runner is trustworthy.
pub fn probe(spec: &RunnerSpec, work_dir: &Path) -> anyhow::Result<ProbeResult> {
    let cap = run_capture(&spec.list_cmd, work_dir, spec.timeout_secs)?;
    let listed = cap.stdout.lines().filter(|l| !l.trim().is_empty()).count();
    Ok(ProbeResult {
        // A clean exit that enumerates nothing is NOT a confirmation — an empty
        // collection means the runner didn't actually find this project's tests.
        confirmed: cap.success && listed > 0,
        listed,
        stderr: cap.stderr,
        timed_out: cap.timed_out,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(list_cmd: &str) -> RunnerSpec {
        RunnerSpec {
            framework: "fake".into(),
            install: vec![],
            list_cmd: list_cmd.into(),
            test_cmd: "true".into(),
            test_join: " ".into(),
            shell_quote_tests: false,
            timeout_secs: 10,
            parallel: false,
        }
    }

    #[test]
    fn enumerating_tests_confirms() {
        let dir = tempfile::tempdir().unwrap();
        let p = probe(&spec("printf 'test_a\\ntest_b\\n'"), dir.path()).unwrap();
        assert!(p.confirmed);
        assert_eq!(p.listed, 2);
    }

    #[test]
    fn clean_exit_but_no_tests_is_not_confirmed() {
        let dir = tempfile::tempdir().unwrap();
        let p = probe(&spec("true"), dir.path()).unwrap();
        assert!(!p.confirmed, "empty enumeration must not confirm");
    }

    #[test]
    fn failing_probe_keeps_stderr_as_repair_signal() {
        let dir = tempfile::tempdir().unwrap();
        let p = probe(
            &spec("echo 'ModuleNotFoundError: no_dep' 1>&2; exit 1"),
            dir.path(),
        )
        .unwrap();
        assert!(!p.confirmed);
        assert!(p.stderr.contains("ModuleNotFoundError"), "{}", p.stderr);
    }
}
