//! The real [`TestRunner`]: run a framework command that emits JUnit XML, then
//! read and parse it. Subprocess execution, capture, and the process-group
//! timeout live in [`crate::proc`].

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

use crate::git::shell_quote;
use crate::junit;
use crate::proc::{run_capture, Captured};
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

    /// Run `ids` once, returning the process capture and whatever JUnit XML
    /// landed at `{junit}` (empty string if the runner wrote nothing).
    fn run_once(&self, ids: &[TestId]) -> anyhow::Result<(Captured, String)> {
        // A fresh JUnit target per run, inside a private temp dir. The file must
        // NOT pre-exist: its absence after the run is how we detect a runner that
        // couldn't execute (rather than reading an empty file as "all passed").
        let junit_dir = tempfile::Builder::new()
            .prefix("pirs-junit-")
            .tempdir()
            .context("create JUnit output dir")?;
        let junit_file = junit_dir.path().join("report.xml");
        let junit_path = junit_file.to_string_lossy().into_owned();

        let joined = if self.spec.shell_quote_tests {
            ids.iter()
                .map(|id| shell_quote(id))
                .collect::<Vec<_>>()
                .join(&self.spec.test_join)
        } else {
            ids.join(&self.spec.test_join)
        };
        let cmd = self
            .spec
            .test_cmd
            .replace("{tests}", &joined)
            .replace("{junit}", &junit_path);

        let cap = run_capture(&cmd, &self.work_dir, self.spec.timeout_secs)?;
        if cap.timed_out {
            bail!(
                "test run exceeded {}s budget: {cmd}",
                self.spec.timeout_secs
            );
        }
        let xml = std::fs::read_to_string(&junit_file).unwrap_or_default();
        Ok((cap, xml))
    }
}

/// Pytest reports each node id it can't resolve on its own line
/// (`ERROR: not found: <work_dir>/<relative id>`) even while otherwise
/// refusing to run anything — a real defect seen in cached SWE-bench-lite
/// PASS_TO_PASS lists, where a parametrized id got truncated mid-comma during
/// dataset construction. Extract exactly those ids so the caller can drop
/// them and retry, instead of one unresolvable id poisoning every requested
/// id's result (including the real targets) back to "not collected".
fn unresolvable_ids(stderr: &str, work_dir: &Path) -> Vec<TestId> {
    let prefix = format!("ERROR: not found: {}/", work_dir.display());
    stderr
        .lines()
        .filter_map(|line| line.strip_prefix(&prefix).map(str::to_string))
        .collect()
}

impl TestRunner for CommandRunner {
    fn run(&self, ids: &[TestId], _ring: Ring) -> anyhow::Result<Snapshot> {
        let (cap, xml) = self.run_once(ids)?;

        // The runner must have produced JUnit XML. Its absence (or an empty
        // file) means the run could not execute (env/setup problem) — surface
        // it, never treat the requested tests as silently passing.
        if xml.trim().is_empty() {
            bail!(
                "test command exited {:?} but wrote no JUnit XML: stderr: {}",
                cap.code,
                cap.stderr.trim()
            );
        }
        let cases = junit::parse(&xml).context("parse JUnit output")?;

        // Zero cases despite requesting a non-empty batch means the whole
        // invocation was rejected outright (e.g. pytest's usage-error exit
        // when even one node id can't be resolved) — not that every test
        // genuinely failed to run. If the runner named the unresolvable ids,
        // drop exactly those and retry once so a single bad id can't sink the
        // real targets' signal.
        if cases.is_empty() && !ids.is_empty() {
            let bad = unresolvable_ids(&cap.stderr, &self.work_dir);
            let retry_ids: Vec<TestId> =
                ids.iter().filter(|id| !bad.contains(id)).cloned().collect();
            if !bad.is_empty() && !retry_ids.is_empty() && retry_ids.len() < ids.len() {
                let (_cap2, xml2) = self.run_once(&retry_ids)?;
                if !xml2.trim().is_empty() {
                    let cases2 = junit::parse(&xml2).context("parse retry JUnit output")?;
                    if !cases2.is_empty() {
                        // Snapshot against the ORIGINAL ids: the dropped ones
                        // correctly report as not-collected (they don't
                        // resolve to a real test), the rest reflect the
                        // retry's real outcomes.
                        return Ok(junit::to_snapshot(ids, &cases2, true));
                    }
                }
            }
        }
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
            shell_quote_tests: false,
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
    fn one_unresolvable_id_no_longer_sinks_the_whole_batch() {
        // Regression test for the real SWE-bench-lite defect this session
        // found: a malformed PASS_TO_PASS id (comma-truncated inside a
        // parametrize bracket) made pytest reject the *entire* combined
        // invocation (exit 4, zero JUnit cases) even though the real targets
        // would have run fine alone. The fake runner below mimics exactly
        // that shape: if the bad id is in {tests}, report it as "not found"
        // on stderr and emit an empty (zero-case) JUnit report; otherwise
        // (the retry, with the bad id filtered out) run for real.
        let dir = tempfile::tempdir().unwrap();
        let work_dir = dir.path().to_string_lossy().into_owned();
        let cmd = format!(
            r#"if echo "{{tests}}" | grep -q "m::bad"; then
                echo "ERROR: not found: {work_dir}/m::bad" 1>&2
                cat > {{junit}} <<'EOF'
<testsuite tests="0"></testsuite>
EOF
            else
                cat > {{junit}} <<'EOF'
<testsuite><testcase classname="m" name="test_ok"/><testcase classname="m" name="test_target"><failure message="x"/></testcase></testsuite>
EOF
            fi"#
        );
        let runner = CommandRunner::new(spec(&cmd, 30), dir.path().to_path_buf());
        let ids = vec![
            "m::test_ok".to_string(),
            "m::test_target".to_string(),
            "m::bad".to_string(),
        ];
        let snap = runner.run(&ids, Ring::Inner).unwrap();
        // The real targets show their true outcome from the retry...
        assert_eq!(snap.get("m::test_ok"), Some(Pass));
        assert_eq!(snap.get("m::test_target"), Some(Fail));
        // ...and the unresolvable id correctly reports as not collected,
        // rather than every id (including the real targets) reporting that.
        assert_eq!(snap.get("m::bad"), Some(NotCollected));
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
    fn shell_quote_tests_survives_ids_with_shell_metacharacters() {
        // Regression test: a real SWE-bench pytest id from a parametrized test
        // (`test_x[('fixt', 'val')]`) has an odd number of quotes. Run via a raw
        // shell command with `sh -c`, an unescaped id corrupts the whole command
        // for every id in the batch — not a parse error on just that one id.
        let dir = tempfile::tempdir().unwrap();
        let xml = r#"<testsuite><testcase classname="m" name="a"/></testsuite>"#;
        let cmd =
            format!("printf '%s' \"{{tests}}\" > joined.txt; cat > {{junit}} <<'EOF'\n{xml}\nEOF");
        let mut s = spec(&cmd, 30);
        s.shell_quote_tests = true;
        let runner = CommandRunner::new(s, dir.path().to_path_buf());
        let tricky = "testing/python/fixtures.py::test_x[('fixt', 'val')]".to_string();
        runner
            .run(&["a".to_string(), tricky.clone()], Ring::Inner)
            .unwrap();
        let joined = std::fs::read_to_string(dir.path().join("joined.txt")).unwrap();
        assert_eq!(joined, format!("'a' '{}'", tricky.replace('\'', "'\\''")));
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
