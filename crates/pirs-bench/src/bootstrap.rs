//! Environment bootstrap. The success criterion is deliberately **not "the
//! suite is green"** — it is "a usable pass/fail signal exists," i.e. the runner
//! can install and enumerate tests. Install commands are best-effort; the probe
//! is the gate. A failure carries the probe's stderr as the repair signal rather
//! than a bare "it didn't work."

use std::path::Path;

use crate::probe::{probe, ProbeResult};
use crate::proc::run_capture;
use crate::types::RunnerSpec;

pub enum Bootstrap {
    /// The runner is installed and can enumerate tests.
    Ready(ProbeResult),
    /// The environment could not be made usable; the string is the repair hint.
    Failed(String),
}

/// Run the spec's install commands (tolerating individual failures) then
/// probe-confirm the runner. Installs are best-effort because many succeed only
/// partially yet still leave a runnable environment; the probe is what decides.
pub fn bootstrap(spec: &RunnerSpec, work_dir: &Path) -> anyhow::Result<Bootstrap> {
    for cmd in &spec.install {
        // Best-effort: a failed install is only fatal if the probe then fails.
        let _ = run_capture(cmd, work_dir, spec.timeout_secs)?;
    }
    let p = probe(spec, work_dir)?;
    if p.confirmed {
        Ok(Bootstrap::Ready(p))
    } else {
        let hint = if p.timed_out {
            "runner probe timed out during bootstrap".to_string()
        } else if p.stderr.trim().is_empty() {
            "runner installed but enumerated no tests".to_string()
        } else {
            p.stderr
        };
        Ok(Bootstrap::Failed(hint))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(install: Vec<&str>, list_cmd: &str) -> RunnerSpec {
        RunnerSpec {
            framework: "fake".into(),
            install: install.into_iter().map(String::from).collect(),
            list_cmd: list_cmd.into(),
            test_cmd: "true".into(),
            test_join: " ".into(),
            timeout_secs: 10,
            parallel: false,
        }
    }

    #[test]
    fn install_then_enumerate_is_ready() {
        let dir = tempfile::tempdir().unwrap();
        // Install writes a marker; list_cmd enumerates a test.
        let s = spec(vec!["touch installed"], "printf 'test_a\\n'");
        match bootstrap(&s, dir.path()).unwrap() {
            Bootstrap::Ready(p) => assert_eq!(p.listed, 1),
            Bootstrap::Failed(h) => panic!("expected ready, got {h}"),
        }
        assert!(dir.path().join("installed").exists());
    }

    #[test]
    fn failed_probe_surfaces_repair_hint() {
        let dir = tempfile::tempdir().unwrap();
        let s = spec(vec![], "echo 'ImportError: missing' 1>&2; exit 1");
        match bootstrap(&s, dir.path()).unwrap() {
            Bootstrap::Failed(hint) => assert!(hint.contains("ImportError"), "{hint}"),
            Bootstrap::Ready(_) => panic!("should not be ready"),
        }
    }

    #[test]
    fn install_failure_tolerated_if_probe_confirms() {
        let dir = tempfile::tempdir().unwrap();
        // First install fails, but the runner still enumerates tests.
        let s = spec(vec!["false", "true"], "printf 'test_a\\n'");
        assert!(matches!(
            bootstrap(&s, dir.path()).unwrap(),
            Bootstrap::Ready(_)
        ));
    }
}
