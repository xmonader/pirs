//! The real [`TestRunner`]: run a framework command that emits JUnit XML, then
//! read and parse it. The command runs in its own process group so a wall-clock
//! timeout can kill the *whole* subprocess tree, not just the shell.

use std::path::PathBuf;
use std::process::Command;
use std::time::Duration;

use anyhow::{bail, Context as _};
use wait_timeout::ChildExt as _;

use crate::junit;
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
            .replace("{tests}", &ids.join(" "))
            .replace("{junit}", &junit_path);

        let mut command = Command::new("sh");
        command.arg("-c").arg(&cmd).current_dir(&self.work_dir);
        // Own process group so the timeout can reap the entire tree.
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt as _;
            command.process_group(0);
        }
        let mut child = command
            .spawn()
            .with_context(|| format!("spawn test command: {cmd}"))?;

        let status = match child.wait_timeout(Duration::from_secs(self.spec.timeout_secs))? {
            Some(status) => status,
            None => {
                kill_tree(&mut child);
                let _ = child.wait();
                bail!(
                    "test run exceeded {}s budget: {cmd}",
                    self.spec.timeout_secs
                );
            }
        };

        // The runner must have produced JUnit XML. Its absence (or an empty
        // file) means the run could not execute (env/setup problem) — surface
        // it, never treat the requested tests as silently passing.
        let xml = std::fs::read_to_string(&junit_file).unwrap_or_default();
        if xml.trim().is_empty() {
            bail!(
                "test command exited {:?} but wrote no JUnit XML to {junit_path}: {cmd}",
                status.code()
            );
        }
        let cases = junit::parse(&xml).context("parse JUnit output")?;
        Ok(junit::to_snapshot(ids, &cases, true))
    }
}

/// Send SIGKILL to the child's process group (negative pid). Falls back to a
/// direct kill on non-unix.
fn kill_tree(child: &mut std::process::Child) {
    #[cfg(unix)]
    {
        let pid = child.id() as i32;
        // Negative pid targets the whole process group we created above.
        unsafe {
            libc::kill(-pid, libc::SIGKILL);
        }
    }
    #[cfg(not(unix))]
    {
        let _ = child.kill();
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
    fn missing_junit_is_an_error_not_a_pass() {
        // A runner that exits 0 but writes no report must NOT yield passes.
        let dir = tempfile::tempdir().unwrap();
        let runner = CommandRunner::new(spec("true", 30), dir.path().to_path_buf());
        let err = runner
            .run(&["m::t".to_string()], Ring::Inner)
            .unwrap_err();
        assert!(err.to_string().contains("no JUnit XML"), "{err}");
    }

    #[test]
    fn timeout_is_enforced() {
        let dir = tempfile::tempdir().unwrap();
        let runner = CommandRunner::new(spec("sleep 30", 1), dir.path().to_path_buf());
        let err = runner
            .run(&["m::t".to_string()], Ring::Inner)
            .unwrap_err();
        assert!(err.to_string().contains("budget"), "{err}");
    }
}
