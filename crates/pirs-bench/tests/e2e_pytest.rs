//! End-to-end proof: a real Python repo with a real bug, fixed by a real file
//! edit, driven through the whole `run_instance` pipeline against actual pytest.
//! No mocks — this exercises discovery, bootstrap, the pytest runner, JUnit
//! parsing, the differential gate, and the reproduce/verify loop together.
//!
//! Skips gracefully (passes with a note) when pytest is not installed, so the
//! suite stays green on machines without a Python toolchain.

use std::path::{Path, PathBuf};
use std::process::Command;

use pirs_bench::gate::Verdict;
use pirs_bench::{run_instance, BaselineCache, DetectorHost, Executor, Instance, Outcome};

fn pytest_available() -> bool {
    Command::new("python3")
        .args(["-m", "pytest", "--version"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// A real executor: on its first attempt it fixes the bug by rewriting the
/// buggy subtraction into an addition in the source file.
struct PatchExecutor {
    source: PathBuf,
    patched: bool,
}
impl Executor for PatchExecutor {
    fn attempt(&mut self, _attempt: u32, _last: Option<&Verdict>) -> anyhow::Result<bool> {
        if self.patched {
            return Ok(false); // nothing more to try
        }
        let text = std::fs::read_to_string(&self.source)?;
        let fixed = text.replace("return a - b", "return a + b");
        assert_ne!(fixed, text, "patch should change the source");
        std::fs::write(&self.source, fixed)?;
        self.patched = true;
        Ok(true)
    }
}

fn write(root: &Path, rel: &str, contents: &str) {
    std::fs::write(root.join(rel), contents).unwrap();
}

#[test]
fn solves_a_real_pytest_bug_end_to_end() {
    if !pytest_available() {
        eprintln!("skipping e2e: pytest not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();

    // A minimal Python project: the pytest detector fires on pyproject.toml.
    write(root, "pyproject.toml", "[project]\nname = \"demo\"\nversion = \"0.0.0\"\n");
    // The bug: subtraction where addition is intended.
    write(root, "mymod.py", "def add(a, b):\n    return a - b\n");
    write(
        root,
        "test_mymod.py",
        "from mymod import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n",
    );

    let host = DetectorHost::with_bundled().unwrap();
    let mut cache = BaselineCache::in_memory();
    let mut exec = PatchExecutor { source: root.join("mymod.py"), patched: false };
    let inst = Instance {
        repo_root: root.to_path_buf(),
        targets: vec!["test_mymod.py::test_add".to_string()],
        keep_green: vec![],
        base_sha: None,
    };

    let outcome = run_instance(&inst, &host, &mut cache, &mut exec, 2).unwrap();
    assert_eq!(outcome, Outcome::Solved, "the real bug should be solved end-to-end");
    assert!(exec.patched, "the executor should have applied its patch");

    // And the fix is actually on disk.
    let final_src = std::fs::read_to_string(root.join("mymod.py")).unwrap();
    assert!(final_src.contains("return a + b"));
}

#[test]
fn unpatched_bug_is_not_a_false_pass() {
    if !pytest_available() {
        eprintln!("skipping e2e: pytest not available");
        return;
    }

    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    write(root, "pyproject.toml", "[project]\nname = \"demo\"\nversion = \"0.0.0\"\n");
    write(root, "mymod.py", "def add(a, b):\n    return a - b\n");
    write(
        root,
        "test_mymod.py",
        "from mymod import add\n\n\ndef test_add():\n    assert add(2, 3) == 5\n",
    );

    // An executor that "tries" but never actually fixes anything.
    struct NoopExecutor;
    impl Executor for NoopExecutor {
        fn attempt(&mut self, _a: u32, _l: Option<&Verdict>) -> anyhow::Result<bool> {
            Ok(true) // claims a change, but the file is untouched
        }
    }

    let host = DetectorHost::with_bundled().unwrap();
    let mut cache = BaselineCache::in_memory();
    let inst = Instance {
        repo_root: root.to_path_buf(),
        targets: vec!["test_mymod.py::test_add".to_string()],
        keep_green: vec![],
        base_sha: None,
    };

    let outcome = run_instance(&inst, &host, &mut cache, &mut NoopExecutor, 2).unwrap();
    // The target never flipped, so it must NOT be reported solved.
    assert_ne!(outcome, Outcome::Solved, "an unfixed bug must never read as solved");
}
