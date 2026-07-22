use std::path::{Path, PathBuf};

use anyhow::bail;

/// Lexically resolve `input` against `cwd` (expanding a leading `~/`).
///
/// This does NOT enforce containment or resolve symlinks — callers that expose
/// a path to the model must use [`resolve_contained`]. Kept public for the few
/// internal call sites that operate on already-trusted paths.
pub fn resolve(cwd: &Path, input: &str) -> PathBuf {
    let expanded = expand_tilde(input);
    let p = Path::new(&expanded);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

/// Resolve `input` against `cwd` and confirm it stays within the allowed root.
///
/// The agent runs LLM-chosen paths through the file tools; without this a
/// prompt-injected model reads `~/.pirs/auth.json` or writes
/// `~/.ssh/authorized_keys` on the host. Containment is enforced against the
/// *canonicalized* nearest-existing ancestor, so a `..`-escape or an in-repo
/// symlink pointing outside the root (`notes -> /etc/passwd`) is rejected, not
/// just lexical escapes.
///
/// Escape hatch: set `PIRS_ALLOW_OUTSIDE_CWD=1` to disable confinement (for the
/// rare legitimate cross-root workflow). The root defaults to `cwd`.
pub fn resolve_contained(cwd: &Path, input: &str) -> anyhow::Result<PathBuf> {
    let lexical = resolve(cwd, input);
    if allow_outside_cwd() {
        // Still prefer the resolved form when the path exists so callers open
        // the same inode we inspected.
        return Ok(canonicalize_existing_prefix(&lexical));
    }
    let root = std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf());
    let resolved = canonicalize_existing_prefix(&lexical);
    if resolved.starts_with(&root) {
        // Return the *canonicalized* path (or longest-existing-prefix form),
        // not the lexical one — otherwise a symlink swap between check and
        // open, or an unresolved `notes -> /etc/passwd` style link that only
        // materializes later, can bypass the containment check.
        Ok(resolved)
    } else {
        bail!(
            "path {} escapes the allowed root {} (set PIRS_ALLOW_OUTSIDE_CWD=1 to permit)",
            input,
            root.display()
        );
    }
}

fn allow_outside_cwd() -> bool {
    std::env::var("PIRS_ALLOW_OUTSIDE_CWD")
        .map(|v| v == "1" || v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// Canonicalize the longest existing ancestor of `p` (resolving `..` and
/// symlinks), then re-append the non-existent tail. Lets us containment-check a
/// path that is about to be created (write/edit) without it existing yet.
fn canonicalize_existing_prefix(p: &Path) -> PathBuf {
    let mut tail: Vec<std::ffi::OsString> = Vec::new();
    let mut ancestor = p;
    loop {
        if let Ok(canon) = std::fs::canonicalize(ancestor) {
            let mut result = canon;
            for comp in tail.iter().rev() {
                result.push(comp);
            }
            return result;
        }
        match (ancestor.file_name(), ancestor.parent()) {
            (Some(name), Some(parent)) => {
                tail.push(name.to_os_string());
                ancestor = parent;
            }
            _ => return p.to_path_buf(),
        }
    }
}

fn expand_tilde(input: &str) -> String {
    if let Some(rest) = input.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return format!("{home}/{rest}");
        }
    }
    input.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn relative_resolves_against_cwd() {
        assert_eq!(
            resolve(Path::new("/work"), "src/main.rs"),
            PathBuf::from("/work/src/main.rs")
        );
    }

    #[test]
    fn absolute_kept() {
        assert_eq!(
            resolve(Path::new("/work"), "/etc/hosts"),
            PathBuf::from("/etc/hosts")
        );
    }

    #[test]
    fn contained_allows_in_root_paths() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("src")).unwrap();
        assert!(resolve_contained(root, "src/main.rs").is_ok());
        assert!(resolve_contained(root, "./a/b/c.txt").is_ok());
    }

    #[test]
    fn contained_returns_canonical_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = root.join("file.txt");
        std::fs::write(&f, b"hi").unwrap();
        let out = resolve_contained(root, "file.txt").unwrap();
        let expect = std::fs::canonicalize(&f).unwrap();
        assert_eq!(out, expect, "must return canonical path, not lexical");
    }

    #[test]
    fn contained_rejects_absolute_escape() {
        let dir = tempfile::tempdir().unwrap();
        assert!(resolve_contained(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn contained_rejects_dotdot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("secret"), b"x").unwrap();
        assert!(resolve_contained(&root, "../secret").is_err());
    }

    #[cfg(unix)]
    #[test]
    fn contained_rejects_symlink_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("outside.txt"), b"secret").unwrap();
        std::os::unix::fs::symlink(dir.path().join("outside.txt"), root.join("link")).unwrap();
        // The symlink target is outside the root -> rejected.
        assert!(resolve_contained(&root, "link").is_err());
    }
}
