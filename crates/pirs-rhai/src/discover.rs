//! Resolving strategies and profiles by *name* — the discovery convention that
//! makes them first-class alongside extensions.
//!
//! An `--strategy`/`--profile` argument is either a literal path to a `.rhai`
//! script or a bare name. A bare name is looked up the same way extensions are
//! discovered ([`crate::ExtensionHost::load_default_dirs_with_trust`]): project
//! `.pirs/` first, then home `~/.pirs/`, under a `strategies/` or `profiles/`
//! subdirectory as `<name>.rhai`. Strategies additionally fall back to the
//! built-ins ([`Strategy::builtin`]); profiles fall back to built-ins
//! (`default`, `weak`).
//!
//! Resolution is a pure function of `(argument, search roots)` — the roots are
//! computed once from cwd + `$HOME`, so the core lookup is testable with
//! explicit temp directories and no environment mutation.

use std::path::{Path, PathBuf};

use anyhow::{bail, Result};

use pirs_agent::profile::Profile;
use pirs_agent::strategy::Strategy;

use crate::profile_script::load_profile_file;
use crate::strategy_script::load_strategy_file;

/// Ordered `.pirs` roots to search for a named strategy/profile: project
/// (`<cwd>/.pirs`) before home (`~/.pirs`). Mirrors extension discovery.
pub fn search_roots(cwd: &Path) -> Vec<PathBuf> {
    let mut roots = vec![cwd.join(".pirs")];
    if let Some(home) = std::env::var_os("HOME") {
        let home_root = Path::new(&home).join(".pirs");
        if !roots.contains(&home_root) {
            roots.push(home_root);
        }
    }
    roots
}

/// Whether the argument denotes a filesystem path (vs. a bare name). A value
/// containing a path separator or ending in `.rhai` is treated as a path and
/// loaded verbatim; anything else is a name to look up in the search roots.
fn looks_like_path(arg: &str) -> bool {
    arg.contains('/') || arg.contains(std::path::MAIN_SEPARATOR) || arg.ends_with(".rhai")
}

/// First existing `<root>/<subdir>/<name>.rhai` across the roots, in order.
fn find_named(roots: &[PathBuf], subdir: &str, name: &str) -> Option<PathBuf> {
    roots.iter().find_map(|root| {
        let candidate = root.join(subdir).join(format!("{name}.rhai"));
        candidate.is_file().then_some(candidate)
    })
}

/// Resolve a strategy from a path or a name, searching the given roots.
/// Name resolution order: `<root>/strategies/<name>.rhai` (each root) →
/// built-in strategy of that name.
pub fn resolve_strategy_in(arg: &str, roots: &[PathBuf]) -> Result<Strategy> {
    if looks_like_path(arg) {
        let path = Path::new(arg);
        if !path.is_file() {
            bail!("strategy script not found: {}", path.display());
        }
        return load_strategy_file(path);
    }
    if let Some(path) = find_named(roots, "strategies", arg) {
        return load_strategy_file(&path);
    }
    if let Some(strategy) = crate::builtins::builtin(arg) {
        return Ok(strategy);
    }
    bail!(
        "unknown strategy {arg:?}: not a built-in ({}) and no {arg}.rhai found under {}",
        crate::builtins::builtin_names().join(", "),
        roots_display(roots, "strategies"),
    )
}

/// Resolve a profile from a path or a name, searching the given roots.
/// Name resolution order: `<root>/profiles/<name>.rhai` (each root) →
/// built-in profile of that name (`default`, `weak`).
pub fn resolve_profile_in(arg: &str, roots: &[PathBuf]) -> Result<Profile> {
    if looks_like_path(arg) {
        let path = Path::new(arg);
        if !path.is_file() {
            bail!("profile script not found: {}", path.display());
        }
        return load_profile_file(path);
    }
    if let Some(path) = find_named(roots, "profiles", arg) {
        return load_profile_file(&path);
    }
    if let Some(profile) = builtin_profile(arg) {
        return Ok(profile);
    }
    bail!(
        "unknown profile {arg:?}: not a built-in ({}) and no {arg}.rhai found under {}",
        builtin_profile_names().join(", "),
        roots_display(roots, "profiles"),
    )
}

/// Built-in profile names (also documented on `--profile`).
pub fn builtin_profile_names() -> Vec<&'static str> {
    vec!["default", "weak"]
}

/// Look up a built-in profile by name.
pub fn builtin_profile(name: &str) -> Option<Profile> {
    match name {
        "default" => crate::profile_script::load_profile_str(
            crate::weak_packs::DEFAULT_PROFILE,
            "default",
        )
        .ok(),
        "weak" => crate::profile_script::load_profile_str(crate::weak_packs::WEAK_PROFILE, "weak")
            .ok(),
        _ => None,
    }
}

/// Profile used to select catalog packs for a session.
///
/// - explicit `--profile` → that profile's `packs`
/// - else → built-in `default` (`packs: "*"`)
///
/// `--weak` does **not** change pack selection (it only composes CLI flags);
/// the default catalog already includes the weak-stack packs. Use
/// `--profile weak` only if you want that role's smaller pack set + persona.
///
/// Does **not** force strategy mode; callers only use the returned `packs`
/// field when loading extensions.
pub fn resolve_pack_profile(profile_arg: Option<&str>, cwd: &Path) -> Result<Profile> {
    resolve_profile(profile_arg.unwrap_or("default"), cwd)
}

/// Resolve a strategy against the default roots derived from `cwd` + `$HOME`.
pub fn resolve_strategy(arg: &str, cwd: &Path) -> Result<Strategy> {
    resolve_strategy_in(arg, &search_roots(cwd))
}

/// Resolve a profile against the default roots derived from `cwd` + `$HOME`.
pub fn resolve_profile(arg: &str, cwd: &Path) -> Result<Profile> {
    resolve_profile_in(arg, &search_roots(cwd))
}

/// `"<root>/<subdir>, ..."` for error messages.
fn roots_display(roots: &[PathBuf], subdir: &str) -> String {
    roots
        .iter()
        .map(|r| r.join(subdir).display().to_string())
        .collect::<Vec<_>>()
        .join(", ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A minimal strategy script the loader accepts (bare map with `phases`).
    fn write_strategy(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join("strategies")).unwrap();
        std::fs::write(
            dir.join("strategies").join(format!("{name}.rhai")),
            format!(
                r#"#{{ name: "{name}", phases: [ #{{ system: "s", prompt: "go", scope: "full" }} ] }}"#
            ),
        )
        .unwrap();
    }

    fn write_profile(dir: &Path, name: &str) {
        std::fs::create_dir_all(dir.join("profiles")).unwrap();
        std::fs::write(
            dir.join("profiles").join(format!("{name}.rhai")),
            format!(
                r#"#{{ name: "{name}", persona: "a helpful reviewer", strategy: "plan-exec" }}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn resolves_named_strategy_from_first_root() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        write_strategy(&root, "custom-strat");
        let s = resolve_strategy_in("custom-strat", &[root]).unwrap();
        assert_eq!(s.name, "custom-strat");
    }

    #[test]
    fn project_root_shadows_home_root() {
        let project = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let proot = project.path().join(".pirs");
        let hroot = home.path().join(".pirs");
        // Same name in both; the project root must win.
        write_strategy(&proot, "dup");
        write_strategy(&hroot, "dup");
        // Make them distinguishable: overwrite the home one with a different name field.
        std::fs::write(
            hroot.join("strategies/dup.rhai"),
            r#"#{ name: "home-version", phases: [ #{ system: "s", prompt: "go", scope: "full" } ] }"#,
        )
        .unwrap();
        let s = resolve_strategy_in("dup", &[proot, hroot]).unwrap();
        assert_eq!(s.name, "dup", "project root should shadow home");
    }

    #[test]
    fn falls_back_to_builtin_strategy() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        // No file for "plan-exec"; the built-in must answer.
        let s = resolve_strategy_in("plan-exec", &[root]).unwrap();
        assert_eq!(s.name, "plan-exec");
    }

    #[test]
    fn named_file_shadows_builtin() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        // A local file named like a built-in overrides the built-in.
        std::fs::create_dir_all(root.join("strategies")).unwrap();
        std::fs::write(
            root.join("strategies/plan-exec.rhai"),
            r#"#{ name: "plan-exec", phases: [ #{ system: "s", prompt: "custom", scope: "full" } ] }"#,
        )
        .unwrap();
        let s = resolve_strategy_in("plan-exec", &[root]).unwrap();
        // The custom file has a single step; the built-in plan-exec has two.
        assert_eq!(s.steps.len(), 1, "local file should shadow the built-in");
    }

    #[test]
    fn unknown_strategy_name_errors_with_context() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        let err = resolve_strategy_in("nope", &[root])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown strategy"), "{err}");
        assert!(err.contains("plan-exec"), "should list built-ins: {err}");
    }

    #[test]
    fn path_argument_loads_verbatim() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("my-strat.rhai");
        std::fs::write(
            &path,
            r#"#{ name: "by-path", phases: [ #{ system: "s", prompt: "go", scope: "full" } ] }"#,
        )
        .unwrap();
        let s = resolve_strategy_in(path.to_str().unwrap(), &[]).unwrap();
        assert_eq!(s.name, "by-path");
    }

    #[test]
    fn missing_path_argument_errors() {
        let err = resolve_strategy_in("does/not/exist.rhai", &[])
            .unwrap_err()
            .to_string();
        assert!(err.contains("not found"), "{err}");
    }

    #[test]
    fn resolves_named_profile() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        write_profile(&root, "reviewer");
        let p = resolve_profile_in("reviewer", &[root]).unwrap();
        assert_eq!(p.name, "reviewer");
    }

    #[test]
    fn falls_back_to_builtin_default_and_weak_profiles() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        let d = resolve_profile_in("default", &[root.clone()]).unwrap();
        assert_eq!(d.name, "default");
        assert_eq!(d.packs, Some(vec!["*".to_string()]));
        let w = resolve_profile_in("weak", &[root]).unwrap();
        assert_eq!(w.name, "weak");
        assert_eq!(w.packs.as_ref().map(|p| p.len()), Some(4));
    }

    #[test]
    fn unknown_profile_name_errors() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join(".pirs");
        let err = resolve_profile_in("ghost", &[root])
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown profile"), "{err}");
        assert!(err.contains("default"), "should list built-ins: {err}");
    }

    #[test]
    fn resolve_pack_profile_defaults_unless_explicit() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let d = resolve_pack_profile(None, cwd).unwrap();
        assert_eq!(d.name, "default");
        assert_eq!(d.packs, Some(vec!["*".to_string()]));
        let explicit = resolve_pack_profile(Some("weak"), cwd).unwrap();
        assert_eq!(explicit.name, "weak");
        assert_eq!(explicit.packs.as_ref().map(|p| p.len()), Some(4));
    }

    #[test]
    fn search_roots_puts_project_before_home() {
        let cwd = Path::new("/tmp/some/project");
        let roots = search_roots(cwd);
        assert_eq!(roots[0], cwd.join(".pirs"));
        // Home root (if $HOME is set) comes after and differs from the project root.
        if roots.len() > 1 {
            assert_ne!(roots[0], roots[1]);
        }
    }
}
