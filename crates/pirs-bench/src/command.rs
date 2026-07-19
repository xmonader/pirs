//! The real [`TestRunner`]: run a framework command that emits JUnit XML, then
//! read and parse it. Subprocess execution, capture, and the process-group
//! timeout live in [`crate::proc`].

use std::path::PathBuf;

use anyhow::{bail, Context as _};

use crate::junit;
use crate::proc::run_capture;
use crate::run::TestRunner;
use crate::types::{Ring, RunnerSpec, Snapshot, TestId};

pub struct CommandRunner {
    pub spec: RunnerSpec,
    pub work_dir: PathBuf,
}

impl CommandRunner {
    pub fn new(spec: RunnerSpec, work_dir: PathBuf) -> Self {
        CommandRunner { spec, work_dir }
    }
}

impl TestRunner for CommandRunner {
    fn run(&self, ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
        // A fresh JUnit target per run, inside a private temp dir. The file must
        // NOT pre-exist: its absence after the run is how we detect a runner that
        // couldn't execute (rather than reading an empty file as "all passed").
        let junit_dir = tempfile::Builder::new()
            .prefix("pirs-junit-")
            .tempdir()
            .context("create JUnit output dir")?;
        let junit_file = junit_dir.path().join("report.xml");
        let junit_path = junit_file.to_string_lossy().into_owned();

        let cmd = self
            .spec
            .test_cmd
            .replace("{tests}", &ids.join(&self.spec.test_join))
            .replace("{junit}", &junit_path);

        let cap = run_capture(&cmd, &self.work_dir, self.spec.timeout_secs)?;
        if cap.timed_out {
            bail!(
                "test run exceeded {}s budget: {cmd}",
                self.spec.timeout_secs
            );
        }

        // The runner must have produced JUnit XML. Its absence (or an empty
        // file) means the run could not execute (env/setup problem) — surface
        // it, never treat the requested tests as silently passing.
        let xml = std::fs::read_to_string(&junit_file).unwrap_or_default();
        if xml.trim().is_empty() {
            bail!(
                "test command exited {:?} but wrote no JUnit XML to {junit_path}: {cmd}\nstderr: {}",
                cap.code,
                cap.stderr.trim()
            );
        }
        let cases = junit::parse(&xml).context("parse JUnit output")?;
        Ok(junit::to_snapshot(ids, &cases, true))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::TestOutcome::*;

    fn spec(test_cmd: &str, timeout: u64) -> RunnerSpec {
        RunnerSpec {
            framework: "fake".into(),
            install: vec![],
            list_cmd: "true".into(),
            test_cmd: test_cmd.into(),
            test_join: " ".into(),
            timeout_secs: timeout,
            parallel: false,
        }
    }

    #[test]
    fn runs_command_and_parses_emitted_junit() {
        // The "runner" is a shell command that writes a fixed JUnit report to
        // {junit} — exercising the real spawn → read → parse → snapshot path.
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<testsuite><testcase classname="m" name="test_ok"/><testcase classname="m" name="test_bad"><failure message="x"/></testcase></testsuite>"#;
        let cmd = format!("cat > {{junit}} <<'EOF'\n{xml}\nEOF");
        let runner = CommandRunner::new(spec(&cmd, 30), dir.path().to_path_buf());
        let ids = vec!["m::test_ok".to_string(), "m::test_bad".to_string()];
        let snap = runner.run(&ids, Ring::Inner).unwrap();
        assert_eq!(snap.get("m::test_ok"), Some(Pass));
        assert_eq!(snap.get("m::test_bad"), Some(Fail));
    }

    #[test]
    fn test_join_controls_how_ids_are_combined() {
        // A runner that records the substituted {tests} to a file, so we can see
        // exactly how the ids were joined.
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<testsuite><testcase classname="m" name="a"/></testsuite>"#;
        let cmd =
            format!("printf '%s' \"{{tests}}\" > joined.txt; cat > {{junit}} <<'EOF'\n{xml}\nEOF");
        let mut s = spec(&cmd, 30);
        s.test_join = "|".into(); // Go-style regex alternation
        let runner = CommandRunner::new(s, dir.path().to_path_buf());
        runner
            .run(&["a".to_string(), "b".to_string()], Ring::Inner)
            .unwrap();
        let joined = std::fs::read_to_string(dir.path().join("joined.txt")).unwrap();
        assert_eq!(joined, "a|b");
    }

    #[test]
    fn missing_junit_is_an_error_not_a_pass() {
        // A runner that exits 0 but writes no report must NOT yield passes.
        let dir = tempfile::tempdir().unwrap();
        let runner = CommandRunner::new(spec("true", 30), dir.path().to_path_buf());
        let err = runner.run(&["m::t".to_string()], Ring::Inner).unwrap_err();
        assert!(err.to_string().contains("no JUnit XML"), "{err}");
    }

    #[test]
    fn timeout_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let runner = CommandRunner::new(spec("sleep 30", 1), dir.path().to_path_buf());
        let err = runner.run(&["m::t".to_string()], Ring::Inner).unwrap_err();
        assert!(err.to_string().contains("budget"), "{err}");
    }
}
