use std::path::{Path, PathBuf};

use anyhow::bail;

use crate::work_context::{current_work_context, parse_named_path};

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

/// Resolve `input` against the session work context (multi-root) with
/// fallback to `cwd` as the primary root.
///
/// The agent runs LLM-chosen paths through the file tools; without this a
/// prompt-injected model reads `~/.pirs/auth.json` or writes
/// `~/.ssh/authorized_keys` on the host. Containment is enforced against the
/// *canonicalized* nearest-existing ancestor, so a `..`-escape or an in-repo
/// symlink pointing outside the roots is rejected.
///
/// Escape hatch: set `PIRS_ALLOW_OUTSIDE_CWD=1` to disable confinement.
///
/// **Multi-root addressing:**
/// - `//name/rel` or `@name/rel` or `name:rel` — pin to a named work root
/// - relative path — try each root (primary first); prefer existing paths
/// - absolute path — allowed only if under some work root
pub fn resolve_contained(cwd: &Path, input: &str) -> anyhow::Result<PathBuf> {
    if allow_outside_cwd() {
        let lexical = resolve(cwd, input);
        return Ok(canonicalize_existing_prefix(&lexical));
    }

    let ctx = current_work_context();
    // Prefer installed multi-root context; if empty (tests / early init), use cwd.
    let roots: Vec<PathBuf> = if ctx.roots.is_empty() {
        vec![std::fs::canonicalize(cwd).unwrap_or_else(|_| cwd.to_path_buf())]
    } else {
        ctx.root_paths()
    };

    // Named root: //backend/src/foo
    if let Some((name, rel)) = parse_named_path(input) {
        let root = ctx.find_by_name(name).ok_or_else(|| {
            let known = ctx.names().join(", ");
            anyhow::anyhow!(
                "unknown work root {name:?} (known: {known}). Use //name/path or @name/path"
            )
        })?;
        return resolve_under_root(&root.path, rel);
    }

    let expanded = expand_tilde(input);
    let p = Path::new(&expanded);

    // Absolute: must land under some root.
    if p.is_absolute() {
        let resolved = canonicalize_existing_prefix(p);
        for root in &roots {
            let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.clone());
            if resolved.starts_with(&root_c) {
                return Ok(resolved);
            }
        }
        bail!(
            "path {} is outside the work context roots [{}] (set PIRS_ALLOW_OUTSIDE_CWD=1 to permit)",
            input,
            roots
                .iter()
                .map(|r| r.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }

    // Relative: try each root. Prefer a path that already exists; else first root
    // (for writes of new files under primary).
    let mut first_ok: Option<PathBuf> = None;
    let mut last_err: Option<anyhow::Error> = None;
    for root in &roots {
        match resolve_under_root(root, &expanded) {
            Ok(path) => {
                if path.exists() || path.parent().map(|p| p.exists()).unwrap_or(false) {
                    // Prefer existing file or existing parent (new file in known dir).
                    if path.exists() {
                        return Ok(path);
                    }
                    if first_ok.is_none() {
                        first_ok = Some(path);
                    }
                } else if first_ok.is_none() {
                    first_ok = Some(path);
                }
            }
            Err(e) => last_err = Some(e),
        }
    }
    if let Some(p) = first_ok {
        return Ok(p);
    }
    Err(last_err.unwrap_or_else(|| {
        anyhow::anyhow!(
            "path {} not found under any work root",
            input
        )
    }))
}

fn resolve_under_root(root: &Path, rel: &str) -> anyhow::Result<PathBuf> {
    let lexical = resolve(root, rel);
    let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let resolved = canonicalize_existing_prefix(&lexical);
    if resolved.starts_with(&root_c) {
        Ok(resolved)
    } else {
        bail!(
            "path {} escapes the allowed root {} (set PIRS_ALLOW_OUTSIDE_CWD=1 to permit)",
            rel,
            root_c.display()
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
    use crate::work_context::{install_work_context, WorkContext};

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
        install_work_context(WorkContext::single(root.to_path_buf()));
        assert!(resolve_contained(root, "src/main.rs").is_ok());
        assert!(resolve_contained(root, "./a/b/c.txt").is_ok());
    }

    #[test]
    fn contained_returns_canonical_existing_path() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let f = root.join("file.txt");
        std::fs::write(&f, b"hi").unwrap();
        install_work_context(WorkContext::single(root.to_path_buf()));
        let out = resolve_contained(root, "file.txt").unwrap();
        let expect = std::fs::canonicalize(&f).unwrap();
        assert_eq!(out, expect, "must return canonical path, not lexical");
    }

    #[test]
    fn contained_rejects_absolute_escape() {
        let dir = tempfile::tempdir().unwrap();
        install_work_context(WorkContext::single(dir.path().to_path_buf()));
        assert!(resolve_contained(dir.path(), "/etc/passwd").is_err());
    }

    #[test]
    fn contained_rejects_dotdot_escape() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("root");
        std::fs::create_dir(&root).unwrap();
        std::fs::write(dir.path().join("secret"), b"x").unwrap();
        install_work_context(WorkContext::single(root.clone()));
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
        install_work_context(WorkContext::single(root.clone()));
        assert!(resolve_contained(&root, "link").is_err());
    }

    #[test]
    fn multi_root_named_path() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("only-b.txt"), b"hi").unwrap();
        let ctx = WorkContext::from_paths(
            a.path().to_path_buf(),
            [b.path().to_path_buf()],
        )
        .unwrap();
        let b_name = ctx.roots[1].name.clone();
        install_work_context(ctx);
        let p = resolve_contained(a.path(), &format!("//{b_name}/only-b.txt")).unwrap();
        assert!(p.ends_with("only-b.txt"));
        assert!(p.starts_with(std::fs::canonicalize(b.path()).unwrap()));
    }

    #[test]
    fn multi_root_relative_finds_secondary() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        std::fs::write(b.path().join("secret-in-b.txt"), b"x").unwrap();
        let ctx =
            WorkContext::from_paths(a.path().to_path_buf(), [b.path().to_path_buf()]).unwrap();
        install_work_context(ctx);
        // File only exists in secondary root — relative lookup should find it.
        let p = resolve_contained(a.path(), "secret-in-b.txt").unwrap();
        assert!(p.ends_with("secret-in-b.txt"));
    }
}
