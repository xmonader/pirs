//! Capability manifests. A pack declares caps in a leading comment line:
//!   // caps: {"exec": ["git"], "fs": [".pirs/**"], "subagents": 2}
//! and the host enforces them at the host-function boundary. Absent caps key
//! = unrestricted for that dimension (backward compatible); present = deny
//! everything not listed. `subagents: 0` denies sub-agents entirely.

use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Caps {
    #[serde(default)]
    pub fs: Option<Vec<String>>,
    #[serde(default)]
    pub exec: Option<Vec<String>>,
    #[serde(default)]
    pub subagents: Option<i64>,
}

impl Caps {
    pub fn is_restricted(&self) -> bool {
        self.fs.is_some() || self.exec.is_some() || self.subagents.is_some()
    }

    /// Human-readable summary for the trust prompt.
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(fs) = &self.fs {
            parts.push(format!("fs=[{}]", fs.join(", ")));
        }
        if let Some(exec) = &self.exec {
            parts.push(format!("exec=[{}]", exec.join(", ")));
        }
        if let Some(n) = self.subagents {
            parts.push(format!("subagents={n}"));
        }
        let base = if parts.is_empty() {
            "no capability manifest — full permissions".to_string()
        } else {
            format!("declares capabilities: {}", parts.join(" "))
        };
        // Surface exec grants that are effectively arbitrary code execution.
        // The allowlist + metachar block can't narrow these — git runs code
        // via `-c core.pager`/aliases, cargo via build scripts, interpreters
        // directly — so we say so plainly instead of implying a tight grant.
        let coarse = coarse_exec_grants(self);
        if coarse.is_empty() {
            base
        } else {
            format!(
                "{base}  ⚠ exec=[{}] can run arbitrary code (flags/subcommands/build scripts) — treat as full shell access",
                coarse.join(", ")
            )
        }
    }
}

/// Binaries whose own flags, subcommands, or build hooks execute arbitrary
/// code, so an `exec` grant for them is inherently full code execution — the
/// allowlist and shell-metachar block cannot narrow it. Used to warn the user
/// at trust time rather than pretend the grant is scoped.
pub const SHELL_CAPABLE_BINARIES: &[&str] = &[
    "git",
    "cargo",
    "make",
    "cmake",
    "npm",
    "npx",
    "yarn",
    "pnpm",
    "node",
    "deno",
    "bun",
    "python",
    "python2",
    "python3",
    "ruby",
    "perl",
    "php",
    "lua",
    "sh",
    "bash",
    "zsh",
    "fish",
    "dash",
    "env",
    "xargs",
    "find",
    "sed",
    "awk",
    "docker",
    "podman",
    "ssh",
    "scp",
    "rsync",
    "gcc",
    "g++",
    "cc",
    "clang",
    "rustc",
    "go",
    "vim",
    "nvim",
    "emacs",
    "gdb",
    "lldb",
    "systemctl",
];

/// The subset of a pack's exec allowlist that grants effectively unrestricted
/// code execution (see `SHELL_CAPABLE_BINARIES`).
pub fn coarse_exec_grants(caps: &Caps) -> Vec<String> {
    let Some(exec) = &caps.exec else {
        return Vec::new();
    };
    exec.iter()
        .filter(|b| {
            let base = b.rsplit('/').next().unwrap_or(b);
            SHELL_CAPABLE_BINARIES.contains(&base)
        })
        .cloned()
        .collect()
}

/// Parse the caps manifest from a script's leading comment lines. Stops at
/// the first non-comment, non-blank line.
pub fn parse_caps(source: &str) -> Caps {
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some(comment) = line.strip_prefix("//") else {
            break;
        };
        let comment = comment.trim();
        if let Some(json) = comment.strip_prefix("caps:") {
            return serde_json::from_str(json.trim()).unwrap_or_default();
        }
    }
    Caps::default()
}

/// Shell metacharacters rejected when an exec allowlist is in force: with
/// these, `exec: ["git"]` would still grant everything (`git ...; rm -rf`).
const EXEC_METACHARS: &[&str] = &["|", ";", "&", ">", "<", "`", "$(", "\n", "\\"];

/// Check a command against the exec capability. Returns Err(reason) when
/// blocked; the reason is shown to the model.
pub fn check_exec(caps: &Caps, command: &str) -> Result<(), String> {
    let Some(allow) = &caps.exec else {
        return Ok(());
    };
    for m in EXEC_METACHARS {
        if command.contains(m) {
            return Err(format!(
                "blocked by capability manifest: shell metacharacter {m:?} not allowed with exec caps"
            ));
        }
    }
    let first = command.split_whitespace().next().unwrap_or("");
    let base = first.rsplit('/').next().unwrap_or(first);
    if allow.iter().any(|a| a == base || a == first) {
        Ok(())
    } else {
        Err(format!(
            "blocked by capability manifest: {base:?} not in exec allowlist [{}]",
            allow.join(", ")
        ))
    }
}

/// Lexically resolve `.`/`..` without touching the filesystem (works for
/// nonexistent paths). Returns `None` if the path is absolute or escapes above
/// the sandbox base via `..` — either of which must be denied regardless of how
/// the pattern is written, so a textual `dir/**` or `*.ext` rule can't be
/// slipped with `dir/../../etc/x` or an absolute `/etc/x.ext`.
fn contained_normal(path: &str) -> Option<String> {
    if path.starts_with('/') {
        return None;
    }
    let mut out: Vec<&str> = Vec::new();
    for comp in path.split('/') {
        match comp {
            "" | "." => {}
            ".." => {
                out.pop()?;
            }
            c => out.push(c),
        }
    }
    Some(out.join("/"))
}

/// Check a path against the fs capability. Patterns: exact match,
/// `dir/**` (prefix), `*.ext` (suffix). The path is first lexically contained
/// (see `contained_normal`); anything absolute or `..`-escaping is denied.
pub fn check_fs(caps: &Caps, path: &str) -> bool {
    let Some(patterns) = &caps.fs else {
        return true;
    };
    let Some(norm) = contained_normal(path) else {
        return false;
    };
    let norm = norm.as_str();
    patterns.iter().any(|p| {
        let p = p.strip_prefix("./").unwrap_or(p);
        if let Some(prefix) = p.strip_suffix("/**") {
            norm == prefix || norm.starts_with(&format!("{prefix}/"))
        } else if let Some(suffix) = p.strip_prefix('*') {
            norm.ends_with(suffix)
        } else {
            norm == p
        }
    })
}

/// subagents: 0 denies; any positive value allows (concurrency caps are not
/// enforced yet — this is a boolean gate).
pub fn subagents_allowed(caps: &Caps) -> bool {
    caps.subagents.map(|n| n > 0).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn check_fs_rejects_parent_traversal_and_absolute() {
        let caps = Caps {
            fs: Some(vec![".pirs/**".to_string()]),
            ..Default::default()
        };
        assert!(check_fs(&caps, ".pirs/ok.txt"));
        assert!(check_fs(&caps, "./.pirs/ok.txt"));
        // Escapes the declared sandbox via `..` — textual prefix match would
        // have allowed these.
        assert!(!check_fs(&caps, ".pirs/../../etc/cron.d/x"));
        assert!(!check_fs(&caps, "/etc/passwd"));
    }

    #[test]
    fn check_fs_suffix_rule_cannot_escape_via_absolute() {
        let caps = Caps {
            fs: Some(vec!["*.log".to_string()]),
            ..Default::default()
        };
        assert!(check_fs(&caps, "app.log"));
        // A suffix rule must not authorize an absolute write just because the
        // extension matches.
        assert!(!check_fs(&caps, "/etc/cron.d/evil.log"));
    }

    #[test]
    fn coarse_exec_grants_are_surfaced_in_summary() {
        // git can exec arbitrary code via `-c core.pager`/aliases; the
        // allowlist can't narrow it, so the trust prompt must say so.
        let git = Caps {
            exec: Some(vec!["git".to_string()]),
            ..Default::default()
        };
        assert_eq!(coarse_exec_grants(&git), vec!["git".to_string()]);
        let s = git.summary();
        assert!(s.contains("arbitrary code"), "{s}");
        assert!(s.contains("full shell access"), "{s}");

        // A path-qualified binary is matched by its base name.
        let qualified = Caps {
            exec: Some(vec!["/usr/bin/bash".to_string()]),
            ..Default::default()
        };
        assert_eq!(
            coarse_exec_grants(&qualified),
            vec!["/usr/bin/bash".to_string()]
        );

        // A narrow, non-shell tool is NOT flagged — no false alarm.
        let narrow = Caps {
            exec: Some(vec!["rg".to_string(), "jq".to_string()]),
            ..Default::default()
        };
        assert!(coarse_exec_grants(&narrow).is_empty());
        assert!(
            !narrow.summary().contains("arbitrary code"),
            "{}",
            narrow.summary()
        );
    }

    #[test]
    fn parses_manifest_from_leading_comment() {
        let src = "// caps: {\"exec\": [\"git\"], \"fs\": [\"./.pirs/**\"], \"subagents\": 2}\nfn f() { 1 }\n";
        let caps = parse_caps(src);
        assert_eq!(caps.exec.as_deref(), Some(&["git".to_string()][..]));
        assert_eq!(caps.subagents, Some(2));
        assert!(caps.is_restricted());

        // No manifest = unrestricted.
        let none = parse_caps("fn f() { 1 }\n");
        assert!(!none.is_restricted());
        assert!(none.summary().contains("full permissions"));
    }

    #[test]
    fn exec_allowlist_blocks_metachars_and_unknown_bins() {
        let caps = Caps {
            exec: Some(vec!["git".into()]),
            ..Default::default()
        };
        assert!(check_exec(&caps, "git status").is_ok());
        assert!(check_exec(&caps, "git log --oneline").is_ok());
        assert!(check_exec(&caps, "git status && rm -rf /").is_err());
        assert!(check_exec(&caps, "git status > /tmp/x").is_err());
        assert!(check_exec(&caps, "echo $(whoami)").is_err());
        assert!(check_exec(&caps, "rm -rf /").is_err());
        assert!(check_exec(&caps, "/usr/bin/git status").is_ok());

        // No manifest: anything goes.
        assert!(check_exec(&Caps::default(), "rm -rf / | cat").is_ok());
    }

    #[test]
    fn fs_globs() {
        let caps = Caps {
            fs: Some(vec![".pirs/**".into(), "*.rhai".into(), "exact.txt".into()]),
            ..Default::default()
        };
        assert!(check_fs(&caps, ".pirs/notes/a.md"));
        assert!(check_fs(&caps, "./.pirs/x"));
        assert!(!check_fs(&caps, "src/main.rs"));
        assert!(check_fs(&caps, "pack.rhai"));
        assert!(check_fs(&caps, "exact.txt"));
        assert!(check_fs(&Caps::default(), "/etc/passwd"));
    }

    #[test]
    fn subagents_gate() {
        assert!(!subagents_allowed(&Caps {
            subagents: Some(0),
            ..Default::default()
        }));
        assert!(subagents_allowed(&Caps {
            subagents: Some(2),
            ..Default::default()
        }));
        assert!(subagents_allowed(&Caps::default()));
    }
}
