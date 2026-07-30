//! Git-backed workspace control: capture the fix as a patch, and roll the tree
//! back to a pristine checkout.
//!
//! Two jobs the harness needs no matter what drives the edits:
//!  - **Patch extraction** — the benchmark deliverable is a unified diff against
//!    the base commit, not our in-place "it's green now." [`diff`](GitWorkspace::diff)
//!    produces exactly that (including newly-created files) without mutating what
//!    the working tree looks like.
//!  - **Rollback** — a failed attempt must not leave partial edits on disk to
//!    poison the next attempt or leak into a diff. [`reset`](GitWorkspace::reset)
//!    returns the tree to the base commit, tracked and untracked alike.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

use crate::proc::run_capture;

/// A git working tree checked out at the task's base commit.
pub struct GitWorkspace {
    root: PathBuf,
    /// Per-git-command wall-clock budget. Generous; git ops are fast but a huge
    /// repo's `clean`/`reset` shouldn't hang forever.
    timeout_secs: u64,
}

impl GitWorkspace {
    pub fn new(root: PathBuf) -> Self {
        GitWorkspace {
            root,
            timeout_secs: 120,
        }
    }

    /// Run a git subcommand in the tree, returning **raw** stdout (no trim).
    ///
    /// Critical for `diff`: unified-diff hunks often end with blank context lines
    /// (`" \n"`). Trimming stdout eats those lines while leaving the `@@ -n,m @@`
    /// counts intact → `git apply` fails with `corrupt patch at line N`. Callers
    /// that need a SHA or single-line value should trim themselves.
    fn git(&self, args: &str) -> anyhow::Result<String> {
        let cmd = format!("git {args}");
        let cap = run_capture(&cmd, &self.root, self.timeout_secs)?;
        if cap.timed_out {
            bail!("git command timed out after {}s: {cmd}", self.timeout_secs);
        }
        if !cap.success {
            bail!(
                "`{cmd}` failed (exit {:?}): {}",
                cap.code,
                cap.stderr.trim()
            );
        }
        Ok(cap.stdout)
    }

    /// Like [`git`] but trims surrounding whitespace — for single-line outputs
    /// (`rev-parse`, etc.), never for patches.
    fn git_trim(&self, args: &str) -> anyhow::Result<String> {
        Ok(self.git(args)?.trim().to_string())
    }

    /// The current HEAD commit SHA — the natural key for the baseline cache.
    pub fn head_sha(&self) -> anyhow::Result<String> {
        self.git_trim("rev-parse HEAD").context("resolve HEAD")
    }

    /// The unified diff of all working-tree changes against HEAD, **including
    /// newly-created files**. Stages everything to capture new files in the diff,
    /// captures `diff --cached`, then unstages so the tree's staged state is left
    /// exactly as it was found (edits stay on disk, index reset).
    ///
    /// The returned patch is pass-through [`sanitize_export_patch`] so agent junk
    /// (`.pirs/`, …) and test-file hunks never leak into SWE-bench oracle applies.
    pub fn diff(&self) -> anyhow::Result<String> {
        self.git("add -A").context("stage for diff")?;
        // Capture with the index staged. `diff --cached HEAD` yields the full
        // patch (edits + new files) that SWE-bench-style evaluation applies.
        // Do NOT trim: trailing blank context lines are part of the hunk body.
        let patch = self.git("diff --cached HEAD").context("compute patch")?;
        // Restore the index to unstaged so we don't leave a surprise staged state.
        self.git("reset -q").context("unstage after diff")?;
        Ok(sanitize_export_patch(&patch))
    }

    /// Return the tree to a pristine base checkout: revert tracked edits and
    /// delete untracked files/dirs the attempt created. After this, `diff()` is
    /// empty.
    pub fn reset(&self) -> anyhow::Result<()> {
        self.git("reset --hard HEAD")
            .context("revert tracked changes")?;
        self.git("clean -fdq").context("remove untracked files")?;
        Ok(())
    }

    /// Restore specific tracked paths to their HEAD version, discarding any edits
    /// to them. Used to keep test files pristine so a fix cannot pass by editing
    /// the tests. Paths that don't exist at HEAD (e.g. an agent-created file) are
    /// skipped rather than erroring. No-op for an empty list.
    pub fn restore_paths(&self, paths: &[&str]) -> anyhow::Result<()> {
        for p in paths {
            // `--` guards against a path that looks like a flag; ignore failures
            // for paths not tracked at HEAD.
            let _ = self.git(&format!("checkout HEAD -- {}", shell_quote(p)));
        }
        Ok(())
    }

    /// Drop every working-tree change that looks like a test file (tracked edits
    /// restored to HEAD; untracked test files removed). Used after agent attempts
    /// so agent-only export cannot smuggle test edits that later collide with the
    /// oracle `test_patch`.
    pub fn scrub_test_like_changes(&self) -> anyhow::Result<()> {
        let status = self.git("status --porcelain").unwrap_or_default();
        let mut restore: Vec<String> = Vec::new();
        let mut remove: Vec<String> = Vec::new();
        for line in status.lines() {
            // porcelain v1: "XY path" or "XY orig -> path" (rename)
            if line.len() < 4 {
                continue;
            }
            let xy = &line[..2];
            let rest = line[3..].trim();
            let path = if let Some((_, dst)) = rest.split_once(" -> ") {
                dst.trim()
            } else {
                rest
            };
            // Unquoted paths; git quotes rare special names — strip quotes.
            let path = path.trim_matches('"');
            if path.is_empty() || !is_likely_test_path(path) {
                continue;
            }
            // Untracked (??) or added (A) not in HEAD → delete; else restore.
            if xy == "??" || xy.ends_with('A') || xy.starts_with('A') {
                remove.push(path.to_string());
            } else {
                restore.push(path.to_string());
            }
        }
        if !restore.is_empty() {
            let refs: Vec<&str> = restore.iter().map(String::as_str).collect();
            let _ = self.restore_paths(&refs);
        }
        for p in remove {
            let full = self.root.join(&p);
            if full.is_dir() {
                let _ = std::fs::remove_dir_all(&full);
            } else {
                let _ = std::fs::remove_file(&full);
            }
        }
        Ok(())
    }
}

/// Minimal single-quote shell escaping for a value embedded in a shell command.
pub(crate) fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// Normalize a `diff --git` path token (`a/foo.py` / `b/foo.py`) to a repo path.
fn diff_path_token(path: &str) -> &str {
    path.strip_prefix("a/")
        .or_else(|| path.strip_prefix("b/"))
        .unwrap_or(path)
        .trim_start_matches("./")
}

/// Paths the agent creates for its own bookkeeping — never part of the gold
/// deliverable. Including them has broken official SWE-bench applies
/// (`patch unexpectedly ends in middle of line` on `.pirs/todos.json`).
fn is_agent_junk_path(path: &str) -> bool {
    let p = diff_path_token(path);
    p.starts_with(".pirs/")
        || p == ".pirs"
        || p.starts_with(".grok/")
        || p == ".grok"
        || p.contains("/.pirs/")
        || p.contains("/.grok/")
}

/// Heuristic: is this path a *test file* (not production source under a package
/// named `test`, e.g. Django's `django/test/utils.py`)?
///
/// SWE-bench grades `test_patch` first, then the model patch — model hunks that
/// touch tests almost always fail to apply (or cheat). Strip them from export.
pub fn is_likely_test_path(path: &str) -> bool {
    let p = diff_path_token(path).replace('\\', "/");
    let pl = p.to_ascii_lowercase();
    let name = pl.rsplit('/').next().unwrap_or(&pl);
    // Directory conventions (SWE-bench django/sympy/sklearn/…).
    if pl.contains("/tests/")
        || pl.starts_with("tests/")
        || pl.contains("/testing/")
        || pl.starts_with("testing/")
    {
        return true;
    }
    // File naming: test_*.py, *_test.py, *_tests.py, tests.py, conftest.py
    if name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_tests.py")
        || name == "tests.py"
        || name == "conftest.py"
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
    {
        return true;
    }
    false
}

/// Drop agent-only and test-file hunks from a unified diff; preserve exact hunk
/// bodies (including trailing blank context lines). Empty input stays empty.
pub fn sanitize_export_patch(patch: &str) -> String {
    if patch.trim().is_empty() {
        return String::new();
    }
    // Split on file headers. Keep the first empty chunk if the patch starts with
    // `diff --git` (standard).
    let mut out = String::new();
    let mut parts = patch.split("diff --git ").peekable();
    // If the patch doesn't start with diff --git, keep a leading preamble.
    if let Some(first) = parts.next() {
        if !first.is_empty() && !patch.starts_with("diff --git ") {
            out.push_str(first);
        } else if !first.is_empty() {
            // leading garbage before first header — rare; keep only if not junk
            out.push_str(first);
        }
    }
    for part in parts {
        if part.is_empty() {
            continue;
        }
        // Header line: `a/path b/path\n...`
        let header_end = part.find('\n').unwrap_or(part.len());
        let header = &part[..header_end];
        // paths are space-separated: `a/foo b/foo` (no spaces in normal git paths)
        let drop = header
            .split_whitespace()
            .any(|tok| is_agent_junk_path(tok) || is_likely_test_path(tok));
        if drop {
            continue;
        }
        out.push_str("diff --git ");
        out.push_str(part);
        if !part.ends_with('\n') {
            out.push('\n');
        }
    }
    // `patch(1)` / SWE-bench apply is picky about EOF; always end with newline
    // when there is content. Do NOT trim trailing blank context lines — only
    // ensure the final character is `\n` if missing.
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Whether `git` is usable in `dir` (a repo with at least one commit). Lets the
/// harness pick the git-backed path only when it applies.
pub fn is_git_repo(dir: &Path) -> bool {
    run_capture("git rev-parse --verify HEAD", dir, 30)
        .map(|c| c.success)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a throwaway git repo with one committed file. Returns None if git
    /// isn't available so the suite still passes on a git-less box.
    fn repo_with_commit() -> Option<tempfile::TempDir> {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let setup = "git init -q && git config user.email t@t && git config user.name t && \
                     printf 'def add(a, b):\\n    return a - b\\n' > mymod.py && \
                     git add -A && git commit -qm base";
        let cap = run_capture(setup, root, 60).ok()?;
        if !cap.success {
            return None;
        }
        Some(dir)
    }

    #[test]
    fn diff_captures_edits_then_reset_reverts_them() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let ws = GitWorkspace::new(dir.path().to_path_buf());

        // No changes yet → empty diff.
        assert!(ws.diff().unwrap().is_empty());

        // Apply a fix and a brand-new file.
        std::fs::write(
            dir.path().join("mymod.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("NEW.txt"), "hello\n").unwrap();

        let patch = ws.diff().unwrap();
        assert!(
            patch.contains("return a + b"),
            "patch must show the edit:\n{patch}"
        );
        assert!(
            patch.contains("NEW.txt"),
            "patch must include the new file:\n{patch}"
        );
        // diff() must not have left a staged index behind.
        let status = run_capture("git status --porcelain", dir.path(), 30).unwrap();
        assert!(
            status.stdout.contains(" M mymod.py"),
            "index should be unstaged: {}",
            status.stdout
        );

        // Reset returns to pristine: edit reverted, new file gone, diff empty.
        ws.reset().unwrap();
        let restored = std::fs::read_to_string(dir.path().join("mymod.py")).unwrap();
        assert!(
            restored.contains("return a - b"),
            "reset must revert the edit"
        );
        assert!(
            !dir.path().join("NEW.txt").exists(),
            "reset must remove untracked files"
        );
        assert!(ws.diff().unwrap().is_empty());
    }

    #[test]
    fn restore_paths_reverts_only_named_files() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        // Add a second committed file so we can prove restore is selective.
        std::fs::write(dir.path().join("other.py"), "x = 1\n").unwrap();
        run_capture("git add -A && git commit -qm second", dir.path(), 60).unwrap();
        let ws = GitWorkspace::new(dir.path().to_path_buf());

        // Edit both files; restore only mymod.py.
        std::fs::write(dir.path().join("mymod.py"), "TAMPERED\n").unwrap();
        std::fs::write(dir.path().join("other.py"), "x = 2\n").unwrap();
        ws.restore_paths(&["mymod.py"]).unwrap();

        // mymod.py is back to its committed content; other.py keeps its edit.
        let restored = std::fs::read_to_string(dir.path().join("mymod.py")).unwrap();
        assert!(
            restored.contains("return a - b"),
            "protected file must be reverted"
        );
        let other = std::fs::read_to_string(dir.path().join("other.py")).unwrap();
        assert_eq!(other, "x = 2\n", "unprotected file must keep its edit");
    }

    #[test]
    fn restore_paths_tolerates_untracked_and_empty() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let ws = GitWorkspace::new(dir.path().to_path_buf());
        // Empty list is a no-op; a path not tracked at HEAD is skipped, not fatal.
        ws.restore_paths(&[]).unwrap();
        ws.restore_paths(&["does/not/exist.py"]).unwrap();
    }

    #[test]
    fn head_sha_is_stable_and_forty_hex() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let ws = GitWorkspace::new(dir.path().to_path_buf());
        let sha = ws.head_sha().unwrap();
        assert_eq!(sha.len(), 40, "expected a full SHA-1: {sha}");
        assert!(sha.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(is_git_repo(dir.path()));
    }

    #[test]
    fn sanitize_drops_pirs_todos_and_keeps_source() {
        let raw = "\
diff --git a/.pirs/todos.json b/.pirs/todos.json
new file mode 100644
index 0000000..fc69ce2
--- /dev/null
+++ b/.pirs/todos.json
@@ -0,0 +1,3 @@
+{
+  \"items\": []
+}
\\ No newline at end of file
diff --git a/django/utils/autoreload.py b/django/utils/autoreload.py
index 25c3b44..d8b0f68 100644
--- a/django/utils/autoreload.py
+++ b/django/utils/autoreload.py
@@ -139,7 +139,7 @@ def iter_modules_and_files(modules, extra_files):
-        except FileNotFoundError:
+        except (FileNotFoundError, ValueError):
";
        let clean = sanitize_export_patch(raw);
        assert!(
            !clean.contains(".pirs"),
            "agent junk must be stripped:\n{clean}"
        );
        assert!(
            clean.contains("django/utils/autoreload.py"),
            "real source must remain:\n{clean}"
        );
        assert!(
            clean.ends_with('\n'),
            "export patch must end with newline for patch(1)"
        );
        assert!(sanitize_export_patch("").is_empty());
        assert!(sanitize_export_patch("   \n").is_empty());
    }

    #[test]
    fn sanitize_drops_test_file_hunks() {
        let raw = "\
diff --git a/django/forms/formsets.py b/django/forms/formsets.py
index a89c355..1b0d455 100644
--- a/django/forms/formsets.py
+++ b/django/forms/formsets.py
@@ -1,1 +1,1 @@
-old
+new
diff --git a/tests/admin_views/tests.py b/tests/admin_views/tests.py
index 880ba0b..2d5bf68 100644
--- a/tests/admin_views/tests.py
+++ b/tests/admin_views/tests.py
@@ -1,1 +1,1 @@
-t1
+t2
diff --git a/test_http404_convert.py b/test_http404_convert.py
new file mode 100644
index 0000000..25c9918
--- /dev/null
+++ b/test_http404_convert.py
@@ -0,0 +1,1 @@
+print('hi')
";
        let clean = sanitize_export_patch(raw);
        assert!(
            clean.contains("django/forms/formsets.py"),
            "source must remain:\n{clean}"
        );
        assert!(
            !clean.contains("tests/admin_views"),
            "tests/ hunk must be stripped:\n{clean}"
        );
        assert!(
            !clean.contains("test_http404_convert"),
            "root test_*.py must be stripped:\n{clean}"
        );
        // Production under django/test/ must NOT be treated as a test file.
        assert!(!is_likely_test_path("django/test/utils.py"));
        assert!(is_likely_test_path("tests/admin_views/tests.py"));
        assert!(is_likely_test_path("test_http404_convert.py"));
    }

    #[test]
    fn sanitize_preserves_trailing_blank_context_lines() {
        // Repro of the strict-mode "corrupt patch at line N" bug: hunk ends with
        // a blank context line (`" \n"`). Trimming the whole patch must not eat it.
        let raw = "\
diff --git a/astropy/modeling/separable.py b/astropy/modeling/separable.py
index a308e27..45bea36 100644
--- a/astropy/modeling/separable.py
+++ b/astropy/modeling/separable.py
@@ -242,7 +242,7 @@ def _cstack(left, right):
         cright = _coord_matrix(right, 'right', noutp)
     else:
         cright = np.zeros((noutp, right.shape[1]))
-        cright[-right.shape[0]:, -right.shape[1]:] = 1
+        cright[-right.shape[0]:, -right.shape[1]:] = right
 
     return np.hstack([cleft, cright])
 
";
        // Confirm raw has the trailing blank context (space-only lines).
        assert!(
            raw.ends_with(" \n"),
            "fixture must end with blank context line"
        );
        let clean = sanitize_export_patch(raw);
        // Count body lines after @@ for old/new
        let body: Vec<&str> = clean
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .skip(1)
            .collect();
        let (mut oc, mut nc) = (0usize, 0usize);
        for l in &body {
            match l.chars().next().unwrap_or(' ') {
                ' ' => {
                    oc += 1;
                    nc += 1;
                }
                '-' => oc += 1,
                '+' => nc += 1,
                _ => {}
            }
        }
        assert_eq!(oc, 7, "old count must stay 7 after sanitize:\n{clean}");
        assert_eq!(nc, 7, "new count must stay 7 after sanitize:\n{clean}");
        // And a naive trim would destroy this:
        let trimmed = raw.trim().to_string() + "\n";
        let tbody: Vec<&str> = trimmed
            .lines()
            .skip_while(|l| !l.starts_with("@@"))
            .skip(1)
            .collect();
        let (mut toc, mut tnc) = (0usize, 0usize);
        for l in &tbody {
            match l.chars().next().unwrap_or(' ') {
                ' ' => {
                    toc += 1;
                    tnc += 1;
                }
                '-' => toc += 1,
                '+' => tnc += 1,
                _ => {}
            }
        }
        assert!(
            toc != 7 || tnc != 7,
            "trim must break hunk counts (documents the bug)"
        );
    }

    #[test]
    fn diff_preserves_trailing_blank_context_for_git_apply() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        // File with a blank line after the body so the hunk includes trailing
        // blank context — the shape that used to be corrupted by stdout.trim().
        std::fs::write(
            dir.path().join("mymod.py"),
            "def add(a, b):\n    return a - b\n\n",
        )
        .unwrap();
        run_capture("git add -A && git commit -qm blank", dir.path(), 60).unwrap();
        let ws = GitWorkspace::new(dir.path().to_path_buf());
        std::fs::write(
            dir.path().join("mymod.py"),
            "def add(a, b):\n    return a + b\n\n",
        )
        .unwrap();
        let patch = ws.diff().unwrap();
        assert!(
            patch.contains("return a + b"),
            "patch must include edit:\n{patch}"
        );
        // Apply on a clean tree must succeed (the whole point of this fix).
        ws.reset().unwrap();
        let patch_path = dir.path().join("fix.patch");
        std::fs::write(&patch_path, &patch).unwrap();
        let apply = run_capture("git apply --whitespace=nowarn fix.patch", dir.path(), 30).unwrap();
        assert!(
            apply.success,
            "git apply must accept export patch: stderr={} patch=\n{patch}",
            apply.stderr
        );
        let body = std::fs::read_to_string(dir.path().join("mymod.py")).unwrap();
        assert!(body.contains("return a + b"), "apply must land edit");
    }

    #[test]
    fn scrub_test_like_removes_test_edits_keeps_source() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(
            dir.path().join("tests/test_add.py"),
            "def test_add():\n    pass\n",
        )
        .unwrap();
        run_capture("git add -A && git commit -qm tests", dir.path(), 60).unwrap();
        let ws = GitWorkspace::new(dir.path().to_path_buf());
        std::fs::write(
            dir.path().join("mymod.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("tests/test_add.py"),
            "def test_add():\n    assert False\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("test_scratch.py"), "print(1)\n").unwrap();
        ws.scrub_test_like_changes().unwrap();
        let src = std::fs::read_to_string(dir.path().join("mymod.py")).unwrap();
        assert!(src.contains("return a + b"), "source edit must stay");
        let test = std::fs::read_to_string(dir.path().join("tests/test_add.py")).unwrap();
        assert!(test.contains("pass"), "tracked test must be restored");
        assert!(
            !dir.path().join("test_scratch.py").exists(),
            "untracked test_*.py must be removed"
        );
    }

    #[test]
    fn diff_excludes_pirs_dir_from_export() {
        let Some(dir) = repo_with_commit() else {
            eprintln!("skipping: git unavailable");
            return;
        };
        let ws = GitWorkspace::new(dir.path().to_path_buf());
        std::fs::create_dir_all(dir.path().join(".pirs")).unwrap();
        std::fs::write(dir.path().join(".pirs/todos.json"), "{\"items\":[]}\n").unwrap();
        std::fs::write(
            dir.path().join("mymod.py"),
            "def add(a, b):\n    return a + b\n",
        )
        .unwrap();
        let patch = ws.diff().unwrap();
        assert!(
            !patch.contains(".pirs"),
            "export must not include .pirs:\n{patch}"
        );
        assert!(
            patch.contains("return a + b"),
            "export must include real edit:\n{patch}"
        );
    }
}
