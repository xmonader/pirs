//! Self-test: generate small, real, buggy projects and run the full harness over
//! them with a deterministic "oracle" fix. This validates the *pipeline*
//! (discovery, bootstrap, baseline, reproduce, differential verify, concentric
//! rings, git patch/rollback) across many project shapes — no model, no API key,
//! fully reproducible. It doubles as an install self-check: `pirs-bench selftest`.
//!
//! The oracle applies the known fix for each generated bug, so an accepted
//! outcome proves the harness correctly detected the runner, reproduced the
//! failure, verified the red→green flip, and extracted the patch. Anything less
//! than a full solve rate is a real harness defect on that project shape.

use std::path::{Path, PathBuf};
use std::process::Command;

use std::sync::Arc;

use anyhow::{bail, Context as _};
use pirs_ai::LlmProvider;
use pirs_bench::{
    run_instance, Attribution, BaselineCache, DetectorHost, Executor, GitWorkspace, Instance, Verdict,
};

use crate::metrics::UsageByModel;
use crate::{build_provider, AgentConfig, AgentExecutor, Provider};

/// How the self-test's "fix" step is driven.
pub enum Mode {
    /// Deterministic known-good fix — validates the harness pipeline offline.
    Oracle,
    /// The real pirs agent — validates the whole thing end to end (needs an
    /// LLM backend + key).
    Agent { provider: Provider, model: String, api_key: String, max_turns: usize },
}

/// A scripted edit the oracle applies: replace the first `find` in `file` with
/// `replace`.
#[derive(Clone)]
pub struct OracleEdit {
    pub file: String,
    pub find: String,
    pub replace: String,
}

/// A deterministic [`Executor`] that applies a fixed set of edits once. Stands in
/// for the model so the harness can be exercised offline.
pub struct OracleExecutor {
    root: PathBuf,
    edits: Vec<OracleEdit>,
    applied: bool,
}

impl OracleExecutor {
    pub fn new(root: PathBuf, edits: Vec<OracleEdit>) -> Self {
        OracleExecutor { root, edits, applied: false }
    }
}

impl Executor for OracleExecutor {
    fn attempt(&mut self, _attempt: u32, _last: Option<&Verdict>) -> anyhow::Result<bool> {
        if self.applied {
            return Ok(false); // nothing more to try
        }
        self.applied = true;
        let mut changed = false;
        for e in &self.edits {
            let path = self.root.join(&e.file);
            let src = std::fs::read_to_string(&path)
                .with_context(|| format!("oracle read {path:?}"))?;
            if let Some(pos) = src.find(&e.find) {
                let mut fixed = String::with_capacity(src.len());
                fixed.push_str(&src[..pos]);
                fixed.push_str(&e.replace);
                fixed.push_str(&src[pos + e.find.len()..]);
                std::fs::write(&path, fixed).with_context(|| format!("oracle write {path:?}"))?;
                changed = true;
            }
        }
        Ok(changed)
    }
}

/// A generated buggy project with the metadata to run and fix it.
struct Project {
    id: String,
    /// (relative path, contents) — written verbatim.
    files: Vec<(String, String)>,
    targets: Vec<String>,
    keep_green: Vec<String>,
    oracle: Vec<OracleEdit>,
}

fn edit(file: &str, find: &str, replace: &str) -> OracleEdit {
    OracleEdit { file: file.into(), find: find.into(), replace: replace.into() }
}

const PYPROJECT: &str = "[project]\nname = \"demo\"\nversion = \"0.0.0\"\n";

/// The template bank. Each takes the global index (for a unique id) and returns a
/// real, install-free pytest project whose one bug the oracle fixes. Every
/// template stresses a different shape: top-level modules, package layouts,
/// class-based tests, multiple targets, keep-green regression, alternate
/// detection markers (setup.cfg / tox.ini), nested test dirs, and non-arithmetic
/// bugs.
const TEMPLATES: &[fn(usize) -> Project] = &[
    // A — top-level module, top-level function test, arithmetic bug.
    |i| Project {
        id: format!("proj-{i:03}-toplevel"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            ("calc.py".into(), "def compute(a, b):\n    return a - b\n".into()),
            (
                "test_calc.py".into(),
                "from calc import compute\n\n\ndef test_compute():\n    assert compute(2, 3) == 5\n".into(),
            ),
        ],
        targets: vec!["test_calc.py::test_compute".into()],
        keep_green: vec![],
        oracle: vec![edit("calc.py", "a - b", "a + b")],
    },
    // B — class-based test (JUnit classname carries the class).
    |i| Project {
        id: format!("proj-{i:03}-class"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            ("strutil.py".into(), "def shout(s):\n    return s.lower()\n".into()),
            (
                "test_strutil.py".into(),
                "from strutil import shout\n\n\nclass TestShout:\n    def test_upper(self):\n        assert shout(\"hi\") == \"HI\"\n".into(),
            ),
        ],
        targets: vec!["test_strutil.py::TestShout::test_upper".into()],
        keep_green: vec![],
        oracle: vec![edit("strutil.py", "s.lower()", "s.upper()")],
    },
    // C — package under src/, path added via conftest (import shape).
    |i| Project {
        id: format!("proj-{i:03}-package"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            (
                "conftest.py".into(),
                "import os, sys\nsys.path.insert(0, os.path.join(os.path.dirname(__file__), \"src\"))\n".into(),
            ),
            ("src/pkg/__init__.py".into(), "".into()),
            ("src/pkg/core.py".into(), "def scale(x):\n    return x * 0\n".into()),
            (
                "tests/test_core.py".into(),
                "from pkg.core import scale\n\n\ndef test_scale():\n    assert scale(3) == 6\n".into(),
            ),
        ],
        targets: vec!["tests/test_core.py::test_scale".into()],
        keep_green: vec![],
        oracle: vec![edit("src/pkg/core.py", "x * 0", "x * 2")],
    },
    // D — one bug, two failing targets.
    |i| Project {
        id: format!("proj-{i:03}-multi"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            ("math2.py".into(), "def dbl(x):\n    return x + x + 1\n".into()),
            (
                "test_math2.py".into(),
                "from math2 import dbl\n\n\ndef test_a():\n    assert dbl(2) == 4\n\n\ndef test_b():\n    assert dbl(5) == 10\n".into(),
            ),
        ],
        targets: vec!["test_math2.py::test_a".into(), "test_math2.py::test_b".into()],
        keep_green: vec![],
        oracle: vec![edit("math2.py", "x + x + 1", "x + x")],
    },
    // E — keep-green regression: fix one function without breaking its twin.
    |i| Project {
        id: format!("proj-{i:03}-keepgreen"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            (
                "ops.py".into(),
                "def inc(x):\n    return x - 1\n\n\ndef dec(x):\n    return x - 1\n".into(),
            ),
            (
                "test_ops.py".into(),
                "from ops import inc, dec\n\n\ndef test_inc():\n    assert inc(5) == 6\n\n\ndef test_dec():\n    assert dec(5) == 4\n".into(),
            ),
        ],
        targets: vec!["test_ops.py::test_inc".into()],
        keep_green: vec!["test_ops.py::test_dec".into()],
        oracle: vec![edit("ops.py", "def inc(x):\n    return x - 1", "def inc(x):\n    return x + 1")],
    },
    // F — detection via setup.cfg only (no pyproject).
    |i| Project {
        id: format!("proj-{i:03}-setupcfg"),
        files: vec![
            ("setup.cfg".into(), "[metadata]\nname = demo\nversion = 0.0.0\n".into()),
            ("thing.py".into(), "def area(w, h):\n    return w + h\n".into()),
            (
                "test_thing.py".into(),
                "from thing import area\n\n\ndef test_area():\n    assert area(3, 4) == 12\n".into(),
            ),
        ],
        targets: vec!["test_thing.py::test_area".into()],
        keep_green: vec![],
        oracle: vec![edit("thing.py", "w + h", "w * h")],
    },
    // G — detection via tox.ini only.
    |i| Project {
        id: format!("proj-{i:03}-tox"),
        files: vec![
            ("tox.ini".into(), "[tox]\nenvlist = py3\n".into()),
            ("widget.py".into(), "def price(n):\n    return n * 10 - 1\n".into()),
            (
                "test_widget.py".into(),
                "from widget import price\n\n\ndef test_price():\n    assert price(4) == 40\n".into(),
            ),
        ],
        targets: vec!["test_widget.py::test_price".into()],
        keep_green: vec![],
        oracle: vec![edit("widget.py", "n * 10 - 1", "n * 10")],
    },
    // H — nested test directory.
    |i| Project {
        id: format!("proj-{i:03}-nested"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            (
                "conftest.py".into(),
                "import os, sys\nsys.path.insert(0, os.path.dirname(__file__))\n".into(),
            ),
            ("deep.py".into(), "def flip(b):\n    return b\n".into()),
            (
                "tests/unit/test_deep.py".into(),
                "from deep import flip\n\n\ndef test_flip():\n    assert flip(True) is False\n".into(),
            ),
        ],
        targets: vec!["tests/unit/test_deep.py::test_flip".into()],
        keep_green: vec![],
        oracle: vec![edit("deep.py", "return b\n", "return not b\n")],
    },
    // I — comparison-operator bug (boundary).
    |i| Project {
        id: format!("proj-{i:03}-compare"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            ("age.py".into(), "def is_adult(age):\n    return age > 18\n".into()),
            (
                "test_age.py".into(),
                "from age import is_adult\n\n\ndef test_boundary():\n    assert is_adult(18) is True\n".into(),
            ),
        ],
        targets: vec!["test_age.py::test_boundary".into()],
        keep_green: vec![],
        oracle: vec![edit("age.py", "age > 18", "age >= 18")],
    },
    // J — loop accumulation bug (assignment vs augmented assignment).
    |i| Project {
        id: format!("proj-{i:03}-loop"),
        files: vec![
            ("pyproject.toml".into(), PYPROJECT.into()),
            (
                "agg.py".into(),
                "def total(xs):\n    s = 0\n    for x in xs:\n        s = x\n    return s\n".into(),
            ),
            (
                "test_agg.py".into(),
                "from agg import total\n\n\ndef test_total():\n    assert total([1, 2, 3]) == 6\n".into(),
            ),
        ],
        targets: vec!["test_agg.py::test_total".into()],
        keep_green: vec![],
        oracle: vec![edit("agg.py", "        s = x\n", "        s += x\n")],
    },
];

/// A generic issue statement handed to the agent (oracle mode ignores it).
fn issue_for(proj: &Project) -> String {
    format!(
        "One or more tests fail because of a bug in this project's SOURCE code. \
         Find and fix the bug in the source so these tests pass. Do NOT modify the tests.\n\n\
         Failing tests:\n{}",
        proj.targets.iter().map(|t| format!("- {t}")).collect::<Vec<_>>().join("\n")
    )
}

/// Generate and run `count` projects under `dir`, returning the attribution and
/// the list of failed ids. Deterministic in [`Mode::Oracle`]; safe to re-run
/// (each project dir is recreated).
pub fn run_selftest(dir: &Path, count: usize, mode: &Mode) -> anyhow::Result<SelftestReport> {
    std::fs::create_dir_all(dir).with_context(|| format!("create {dir:?}"))?;
    let host = DetectorHost::with_bundled().context("load detectors")?;
    // In agent mode, build the provider once and share it across executors.
    let provider: Option<Arc<dyn LlmProvider>> = match mode {
        Mode::Agent { provider, .. } => Some(build_provider(provider)),
        Mode::Oracle => None,
    };
    let mut attribution = Attribution::new();
    let mut failures = Vec::new();
    let mut total_usage = UsageByModel::default();

    for i in 0..count {
        let proj = TEMPLATES[i % TEMPLATES.len()](i);
        let root = dir.join(&proj.id);
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root)?;
        for (rel, content) in &proj.files {
            let path = root.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::write(&path, content).with_context(|| format!("write {path:?}"))?;
        }
        git_init_commit(&root)?;

        let ws = GitWorkspace::new(root.clone());
        let mut cache = BaselineCache::in_memory();
        let inst = Instance {
            repo_root: root.clone(),
            targets: proj.targets.clone(),
            keep_green: proj.keep_green.clone(),
            base_sha: None,
        };

        // Build the fix step (oracle or real agent). `Option`s keep the concrete
        // agent reachable after the run so we can read its per-session metrics.
        let mut oracle: Option<OracleExecutor> = None;
        let mut agent: Option<AgentExecutor> = None;
        match mode {
            Mode::Oracle => {
                oracle = Some(OracleExecutor::new(root.clone(), proj.oracle.clone()));
            }
            Mode::Agent { model, api_key, max_turns, .. } => {
                agent = Some(AgentExecutor::new(
                    root.clone(),
                    issue_for(&proj),
                    proj.targets.clone(),
                    proj.keep_green.clone(),
                    AgentConfig {
                        model: model.clone(),
                        api_key: api_key.clone(),
                        max_turns_per_attempt: *max_turns,
                        provider: Arc::clone(provider.as_ref().expect("provider in agent mode")),
                    },
                )?);
            }
        }
        let exec: &mut dyn Executor = match (&mut oracle, &mut agent) {
            (Some(o), _) => o,
            (_, Some(a)) => a,
            _ => unreachable!("one executor is always built"),
        };

        let report = run_instance(&inst, &host, &mut cache, exec, 3, Some(&ws))?;
        attribution.record(&report.outcome);

        let mark = if report.outcome.is_accepted() { "ok " } else { "FAIL" };
        // In agent mode, surface per-session behavior + token cost.
        let extra = match &agent {
            Some(a) => {
                let u = a.session_usage();
                total_usage.merge(&u);
                format!(" | {} | {}", a.session_stats().summary(), UsageByModel::line(&u.total()))
            }
            None => String::new(),
        };
        eprintln!("[{mark}] {} -> {:?}{extra}", proj.id, report.outcome);
        if !report.outcome.is_accepted() {
            failures.push(format!("{} ({:?})", proj.id, report.outcome));
        }
    }

    Ok(SelftestReport { attribution, failures, usage: total_usage })
}

/// Aggregate outcome of a self-test run.
pub struct SelftestReport {
    pub attribution: Attribution,
    pub failures: Vec<String>,
    pub usage: UsageByModel,
}

/// `git init` + commit the current tree so `GitWorkspace` has a base to diff and
/// reset against. Local config so it works on machines without a global identity.
fn git_init_commit(root: &Path) -> anyhow::Result<()> {
    let steps: &[&[&str]] = &[
        &["init", "-q"],
        &["config", "user.email", "selftest@pirs"],
        &["config", "user.name", "pirs selftest"],
        &["add", "-A"],
        &["commit", "-qm", "base"],
    ];
    for args in steps {
        let out = Command::new("git")
            .args(*args)
            .current_dir(root)
            .output()
            .with_context(|| format!("run git {args:?}"))?;
        if !out.status.success() {
            bail!(
                "git {args:?} failed in {root:?}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    Ok(())
}
