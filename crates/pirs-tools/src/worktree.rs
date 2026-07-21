//! Git worktree session binding (Vibe `--worktree` class, no TUI).
//!
//! Resolves a dedicated worktree path for a named branch and optionally
//! creates/reuses it via `git worktree`.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Sanitize a worktree/branch name for filesystem use.
pub fn sanitize_worktree_name(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let s = s.trim_matches('-').to_string();
    if s.is_empty() {
        "worktree".into()
    } else {
        s
    }
}

/// Default worktree directory under a repo: `{repo}/.pirs/worktrees/{name}`.
pub fn worktree_path_for(repo_root: &Path, name: &str) -> PathBuf {
    repo_root
        .join(".pirs")
        .join("worktrees")
        .join(sanitize_worktree_name(name))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorktreeSession {
    /// Absolute path to use as session cwd.
    pub cwd: PathBuf,
    /// Branch name attached.
    pub branch: String,
    /// Whether this call created a new worktree (vs reuse).
    pub created: bool,
}

/// Resolve git repository root for `start` (or error).
pub fn git_repo_root(start: &Path) -> anyhow::Result<PathBuf> {
    let out = Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(start)
        .output()
        .map_err(|e| anyhow::anyhow!("git not available: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "not a git repository: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let p = String::from_utf8_lossy(&out.stdout).trim().to_string();
    Ok(PathBuf::from(p))
}

/// Create or reuse a worktree for `name` (branch name) under the repo.
///
/// - If the path already exists and is a directory, reuse it (`created=false`).
/// - Else run `git worktree add -B <name> <path>` (create branch if needed).
pub fn ensure_worktree(repo_root: &Path, name: &str) -> anyhow::Result<WorktreeSession> {
    let branch = sanitize_worktree_name(name);
    let cwd = worktree_path_for(repo_root, &branch);
    if cwd.is_dir() {
        // Already present — treat as reuse (git worktree list may also know it).
        return Ok(WorktreeSession {
            cwd: cwd.canonicalize().unwrap_or(cwd),
            branch,
            created: false,
        });
    }
    if let Some(parent) = cwd.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let out = Command::new("git")
        .args([
            "worktree",
            "add",
            "-B",
            &branch,
            cwd.to_str().ok_or_else(|| anyhow::anyhow!("non-utf8 path"))?,
        ])
        .current_dir(repo_root)
        .output()
        .map_err(|e| anyhow::anyhow!("git worktree add failed to spawn: {e}"))?;
    if !out.status.success() {
        anyhow::bail!(
            "git worktree add failed: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let abs = cwd.canonicalize().unwrap_or(cwd);
    // Session cwd must live under the worktree path and differ from repo root
    // when created as a separate worktree.
    Ok(WorktreeSession {
        cwd: abs,
        branch,
        created: true,
    })
}

/// High-level: from current dir + name, bind session cwd to worktree.
pub fn bind_session_worktree(start: &Path, name: &str) -> anyhow::Result<WorktreeSession> {
    let root = git_repo_root(start)?;
    ensure_worktree(&root, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_and_path() {
        assert_eq!(sanitize_worktree_name("feat/foo"), "feat-foo");
        let p = worktree_path_for(Path::new("/repo"), "feat/foo");
        assert_eq!(p, PathBuf::from("/repo/.pirs/worktrees/feat-foo"));
    }

    #[test]
    fn ensure_worktree_create_and_reuse() {
        let dir = tempfile::tempdir().unwrap();
        let repo = dir.path();
        // Init git repo with one commit (worktree add needs a HEAD).
        assert!(Command::new("git")
            .args(["init"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.email", "t@example.com"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
        std::fs::write(repo.join("README"), "hi").unwrap();
        assert!(Command::new("git")
            .args(["add", "README"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());
        assert!(Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(repo)
            .status()
            .unwrap()
            .success());

        let s1 = ensure_worktree(repo, "feature-x").unwrap();
        assert!(s1.created);
        assert!(s1.cwd.is_dir());
        assert_ne!(
            s1.cwd.canonicalize().unwrap(),
            repo.canonicalize().unwrap()
        );
        assert!(s1.cwd.starts_with(repo.join(".pirs").join("worktrees")));

        let s2 = ensure_worktree(repo, "feature-x").unwrap();
        assert!(!s2.created);
        assert_eq!(s1.cwd, s2.cwd);
    }
}
