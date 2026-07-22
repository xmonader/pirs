//! Multi-root work context: one primary cwd + additional allowed roots.
//!
//! Path tools resolve against every root. Named prefixes select a root by
//! directory basename:
//!
//! - `//backend/src/main.rs`  → root named `backend` + `src/main.rs`
//! - `@backend/src/main.rs`   → same
//! - `backend:src/main.rs`    → same (colon form)
//!
//! Relative paths try roots in order (primary first); absolute paths must
//! land under some root.

use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

use anyhow::{bail, Context as _};

static WORK_CONTEXT: OnceLock<Mutex<WorkContext>> = OnceLock::new();

fn store() -> &'static Mutex<WorkContext> {
    WORK_CONTEXT.get_or_init(|| Mutex::new(WorkContext::default()))
}

/// Session work context: primary process cwd + extra roots.
#[derive(Debug, Clone, Default)]
pub struct WorkContext {
    /// Primary root (also process cwd for bash default).
    pub primary: PathBuf,
    /// All roots including primary (canonicalized when installed).
    pub roots: Vec<WorkRoot>,
}

#[derive(Debug, Clone)]
pub struct WorkRoot {
    /// Short name for `//name/…` (directory basename by default).
    pub name: String,
    pub path: PathBuf,
}

impl WorkContext {
    pub fn single(primary: PathBuf) -> Self {
        let name = root_name(&primary);
        let path = canonicalize_root(&primary);
        Self {
            primary: path.clone(),
            roots: vec![WorkRoot { name, path }],
        }
    }

    /// Build from primary + additional directories (deduped).
    pub fn from_paths(primary: PathBuf, also: impl IntoIterator<Item = PathBuf>) -> anyhow::Result<Self> {
        let mut roots = Vec::new();
        let mut seen = std::collections::HashSet::new();

        let push = |roots: &mut Vec<WorkRoot>, seen: &mut std::collections::HashSet<PathBuf>, p: PathBuf| -> anyhow::Result<()> {
            let abs = if p.is_absolute() {
                p
            } else {
                std::env::current_dir()
                    .unwrap_or_else(|_| PathBuf::from("."))
                    .join(p)
            };
            if !abs.is_dir() {
                bail!("{} is not a directory", abs.display());
            }
            let path = canonicalize_root(&abs);
            if !seen.insert(path.clone()) {
                return Ok(());
            }
            let mut name = root_name(&path);
            // Disambiguate duplicate basenames: backend, backend-2, …
            let base = name.clone();
            let mut n = 2u32;
            while roots.iter().any(|r| r.name == name) {
                name = format!("{base}-{n}");
                n += 1;
            }
            roots.push(WorkRoot { name, path });
            Ok(())
        };

        push(&mut roots, &mut seen, primary)?;
        for p in also {
            push(&mut roots, &mut seen, p)?;
        }
        let primary = roots[0].path.clone();
        Ok(Self { primary, roots })
    }

    pub fn primary(&self) -> &Path {
        &self.primary
    }

    pub fn root_paths(&self) -> Vec<PathBuf> {
        self.roots.iter().map(|r| r.path.clone()).collect()
    }

    pub fn names(&self) -> Vec<&str> {
        self.roots.iter().map(|r| r.name.as_str()).collect()
    }

    pub fn find_by_name(&self, name: &str) -> Option<&WorkRoot> {
        self.roots.iter().find(|r| r.name == name)
    }

    /// Human summary for prompts / headers.
    pub fn summary_line(&self) -> String {
        if self.roots.len() <= 1 {
            return format!("cwd: {}", self.primary.display());
        }
        let parts: Vec<String> = self
            .roots
            .iter()
            .map(|r| format!("{}={}", r.name, r.path.display()))
            .collect();
        format!("work context ({} roots): {}", self.roots.len(), parts.join(" · "))
    }

    pub fn prompt_section(&self) -> String {
        if self.roots.len() <= 1 {
            return format!("\nCurrent working directory: {}\n", self.primary.display());
        }
        let mut s = String::from("\nWork context — multiple roots (multi-repo):\n");
        s.push_str(&format!(
            "- Primary (default for relative paths & bash): {}\n",
            self.primary.display()
        ));
        s.push_str("- Additional roots:\n");
        for r in &self.roots {
            let mark = if r.path == self.primary { " (primary)" } else { "" };
            s.push_str(&format!("  - //{} → {}{mark}\n", r.name, r.path.display()));
        }
        s.push_str(
            "- Address a root explicitly: `//name/rel/path`, `@name/rel/path`, or `name:rel/path`.\n\
             - Relative paths are tried against each root (primary first).\n\
             - Absolute paths are allowed only if they stay under some listed root.\n",
        );
        s
    }
}

fn root_name(p: &Path) -> String {
    p.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("root")
        .to_string()
}

fn canonicalize_root(p: &Path) -> PathBuf {
    std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf())
}

/// Install the process-wide work context (called once at session start).
pub fn install_work_context(ctx: WorkContext) {
    *store().lock().unwrap() = ctx;
}

pub fn current_work_context() -> WorkContext {
    store().lock().unwrap().clone()
}

pub fn work_context_summary() -> String {
    store().lock().unwrap().summary_line()
}

/// Parse `//name/rel`, `@name/rel`, `name:rel` → (root_name, relative).
pub fn parse_named_path(input: &str) -> Option<(&str, &str)> {
    let s = input.trim();
    if let Some(rest) = s.strip_prefix("//") {
        let (name, rel) = rest.split_once('/')?;
        if name.is_empty() {
            return None;
        }
        return Some((name, rel));
    }
    if let Some(rest) = s.strip_prefix('@') {
        let (name, rel) = rest.split_once('/')?;
        if name.is_empty() {
            return None;
        }
        return Some((name, rel));
    }
    // name:path — but not Windows drive letters C:\
    if let Some((name, rel)) = s.split_once(':') {
        if name.len() > 1
            && name
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            && !rel.is_empty()
            && !rel.starts_with('\\')
        {
            return Some((name, rel));
        }
    }
    None
}

/// Load named contexts from `~/.pirs/contexts.toml` if present.
///
/// ```toml
/// [[context]]
/// name = "full-stack"
/// roots = ["/home/me/fe", "/home/me/be"]
/// ```
pub fn load_named_context(name: &str) -> anyhow::Result<WorkContext> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let path = PathBuf::from(home).join(".pirs").join("contexts.toml");
    let text = std::fs::read_to_string(&path)
        .with_context(|| format!("read {} (define [[context]] entries)", path.display()))?;
    let value: toml::Value = text.parse().context("parse contexts.toml")?;
    let arr = value
        .get("context")
        .and_then(|v| v.as_array())
        .context("no [[context]] table in contexts.toml")?;
    for entry in arr {
        let n = entry
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if n != name {
            continue;
        }
        let roots = entry
            .get("roots")
            .and_then(|v| v.as_array())
            .context("context.roots must be an array of paths")?;
        let mut paths: Vec<PathBuf> = Vec::new();
        for r in roots {
            let s = r.as_str().context("root path must be a string")?;
            paths.push(expand_user_path(s));
        }
        if paths.is_empty() {
            bail!("context {name:?} has empty roots");
        }
        let primary = paths.remove(0);
        return WorkContext::from_paths(primary, paths);
    }
    bail!(
        "context {name:?} not found in {} — define [[context]] name = \"{name}\" roots = […]",
        path.display()
    );
}

fn expand_user_path(s: &str) -> PathBuf {
    if let Some(rest) = s.strip_prefix("~/") {
        if let Ok(home) = std::env::var("HOME") {
            return PathBuf::from(home).join(rest);
        }
    }
    PathBuf::from(s)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_named_forms() {
        assert_eq!(
            parse_named_path("//backend/src/a.rs"),
            Some(("backend", "src/a.rs"))
        );
        assert_eq!(
            parse_named_path("@backend/src/a.rs"),
            Some(("backend", "src/a.rs"))
        );
        assert_eq!(
            parse_named_path("backend:src/a.rs"),
            Some(("backend", "src/a.rs"))
        );
        assert_eq!(parse_named_path("C:\\windows"), None);
        assert_eq!(parse_named_path("plain/path"), None);
    }

    #[test]
    fn from_paths_dedupes_and_names() {
        let a = tempfile::tempdir().unwrap();
        let b = tempfile::tempdir().unwrap();
        let ctx = WorkContext::from_paths(a.path().to_path_buf(), [b.path().to_path_buf()]).unwrap();
        assert_eq!(ctx.roots.len(), 2);
        assert_eq!(ctx.primary, ctx.roots[0].path);
    }
}
