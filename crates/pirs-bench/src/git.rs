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
        GitWorkspace { root, timeout_secs: 120 }
    }

    /// Run a git subcommand in the tree, returning trimmed stdout. Fails loudly
    /// on a non-zero exit or timeout — a silently-failed reset is the dangerous
    /// case (it would leave edits behind), so we never swallow it.
    fn git(&self, args: &str) -> anyhow::Result<String> {
        let cmd = format!("git {args}");
        let cap = run_capture(&cmd, &self.root, self.timeout_secs)?;
        if cap.timed_out {
            bail!("git command timed out after {}s: {cmd}", self.timeout_secs);
        }
        if !cap.success {
            bail!("`{cmd}` failed (exit {:?}): {}", cap.code, cap.stderr.trim());
        }
        Ok(cap.stdout.trim().to_string())
    }

    /// The current HEAD commit SHA — the natural key for the baseline cache.
    pub fn head_sha(&self) -> anyhow::Result<String> {
        self.git("rev-parse HEAD").context("resolve HEAD")
    }

    /// The unified diff of all working-tree changes against HEAD, **including
    /// newly-created files**. Stages everything to capture new files in the diff,
    /// captures `diff --cached`, then unstages so the tree's staged state is left
    /// exactly as it was found (edits stay on disk, index reset).
    pub fn diff(&self) -> anyhow::Result<String> {
        self.git("add -A").context("stage for diff")?;
        // Capture with the index staged. `diff --cached HEAD` yields the full
        // patch (edits + new files) that SWE-bench-style evaluation applies.
        let patch = self.git("diff --cached HEAD").context("compute patch")?;
        // Restore the index to unstaged so we don't leave a surprise staged state.
        self.git("reset -q").context("unstage after diff")?;
        Ok(patch)
    }

    /// Return the tree to a pristine base checkout: revert tracked edits and
    /// delete untracked files/dirs the attempt created. After this, `diff()` is
    /// empty.
    pub fn reset(&self) -> anyhow::Result<()> {
        self.git("reset --hard HEAD").context("revert tracked changes")?;
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
}

/// Minimal single-quote shell escaping for a path embedded in a git command.
fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
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
        std::fs::write(dir.path().join("mymod.py"), "def add(a, b):\n    return a + b\n").unwrap();
        std::fs::write(dir.path().join("NEW.txt"), "hello\n").unwrap();

        let patch = ws.diff().unwrap();
        assert!(patch.contains("return a + b"), "patch must show the edit:\n{patch}");
        assert!(patch.contains("NEW.txt"), "patch must include the new file:\n{patch}");
        // diff() must not have left a staged index behind.
        let status = run_capture("git status --porcelain", dir.path(), 30).unwrap();
        assert!(status.stdout.contains(" M mymod.py"), "index should be unstaged: {}", status.stdout);

        // Reset returns to pristine: edit reverted, new file gone, diff empty.
        ws.reset().unwrap();
        let restored = std::fs::read_to_string(dir.path().join("mymod.py")).unwrap();
        assert!(restored.contains("return a - b"), "reset must revert the edit");
        assert!(!dir.path().join("NEW.txt").exists(), "reset must remove untracked files");
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
        assert!(restored.contains("return a - b"), "protected file must be reverted");
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
}
