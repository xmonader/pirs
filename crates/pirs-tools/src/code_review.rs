//! Deterministic high-precision code review prep and offline scan.
//!
//! Selection, noise filtering, unit partitioning, tool diet, and structured
//! findings are pure (or git-only) so tests need no LLM. A light heuristic
//! pass can emit findings from changed content offline; agent-driven deep
//! review can consume the same plan + diet later.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Max file size (bytes) included by default; larger blobs are noise-filtered.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 512 * 1024;

/// Tools permitted on a review unit (read-only diet).
pub const REVIEW_ALLOWED_TOOLS: &[&str] = &[
    "read",
    "grep",
    "find",
    "ls",
    "git",
    "code_map",
    "code_search",
    "lsp",
    "doctor",
    "audit_tail",
    "recall",
];

/// Tools that must never run under the default review diet.
pub const REVIEW_DENIED_TOOLS: &[&str] = &[
    "write",
    "edit",
    "edit_block",
    "safe_edit",
    "ast_edit",
    "bash",
    "run_tests",
    "office_document",
    "job_kill",
    "job_steer",
];

#[derive(Debug, Clone, Default)]
pub struct ReviewSelectOpts {
    /// Compare `from..to` (both optional). Empty = dirty workspace.
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_untracked: bool,
    pub max_file_bytes: u64,
}

impl ReviewSelectOpts {
    pub fn dirty_default() -> Self {
        Self {
            from: None,
            to: None,
            include_untracked: true,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }

    pub fn range(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: Some(from.into()),
            to: Some(to.into()),
            include_untracked: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
        }
    }
}

/// One isolated review unit (related paths share a unit).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewUnit {
    pub id: String,
    pub paths: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum FindingSeverity {
    Error,
    Warning,
    Info,
    Nit,
}

impl FindingSeverity {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Error => "error",
            Self::Warning => "warning",
            Self::Info => "info",
            Self::Nit => "nit",
        }
    }
}

/// Structured finding — the primary output shape for review results.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Finding {
    pub path: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub line: Option<u32>,
    pub severity: FindingSeverity,
    pub kind: String,
    pub message: String,
}

impl Finding {
    pub fn is_located(&self) -> bool {
        self.line.is_some()
    }

    pub fn is_empty_message(&self) -> bool {
        self.message.trim().is_empty()
    }
}

/// Full offline review report (plan + findings + diet).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewReport {
    pub mode: String,
    pub files: Vec<String>,
    pub units: Vec<ReviewUnit>,
    pub findings: Vec<Finding>,
    pub tool_diet: ReviewToolDiet,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewToolDiet {
    pub allowed: Vec<String>,
    pub denied: Vec<String>,
    pub mutation_allowed: bool,
}

/// True when a path looks like generated / lockfile / vendor noise.
pub fn is_noise_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    let lower = p.to_ascii_lowercase();
    let name = Path::new(&p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();

    if matches!(
        name.as_str(),
        "cargo.lock"
            | "package-lock.json"
            | "yarn.lock"
            | "pnpm-lock.yaml"
            | "poetry.lock"
            | "composer.lock"
            | "gemfile.lock"
            | "go.sum"
            | "flake.lock"
    ) {
        return true;
    }

    let noise_dirs = [
        "/target/",
        "/node_modules/",
        "/.git/",
        "/dist/",
        "/build/",
        "/.next/",
        "/vendor/",
        "/__pycache__/",
        "/.pirs/",
        "/coverage/",
        "/.tox/",
    ];
    let padded = format!("/{lower}");
    if noise_dirs.iter().any(|d| padded.contains(d)) {
        return true;
    }
    if lower.ends_with(".min.js")
        || lower.ends_with(".min.css")
        || lower.ends_with(".map")
        || lower.ends_with(".pb.go")
        || lower.ends_with("_generated.go")
        || lower.ends_with(".generated.rs")
    {
        return true;
    }
    matches!(
        Path::new(&lower)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or(""),
        "png" | "jpg" | "jpeg" | "gif" | "webp" | "ico" | "pdf" | "zip" | "gz" | "tar" | "wasm"
            | "so" | "dylib" | "dll" | "exe" | "o" | "a"
    )
}

/// Whether `tool` is allowed under the default review diet.
pub fn review_tool_allowed(tool: &str) -> bool {
    let t = tool.trim();
    if REVIEW_DENIED_TOOLS.iter().any(|d| *d == t) {
        return false;
    }
    if crate::safety_profile::is_file_mutation_tool(t) || crate::safety_profile::is_shell_tool(t) {
        return false;
    }
    REVIEW_ALLOWED_TOOLS.iter().any(|a| *a == t) || t.starts_with("lsp")
}

pub fn review_tool_diet() -> ReviewToolDiet {
    ReviewToolDiet {
        allowed: REVIEW_ALLOWED_TOOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        denied: REVIEW_DENIED_TOOLS
            .iter()
            .map(|s| (*s).to_string())
            .collect(),
        mutation_allowed: false,
    }
}

/// Drop empty messages and unlocated findings (precision-first default).
pub fn filter_precision(findings: Vec<Finding>) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| !f.is_empty_message())
        .filter(|f| f.is_located())
        .collect()
}

/// Partition paths into review units by parent directory (locale pairs share a unit).
pub fn partition_units(paths: &[String]) -> Vec<ReviewUnit> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        let key = unit_key(p);
        by_key.entry(key).or_default().push(p.clone());
    }
    let mut units = Vec::new();
    for (i, (_k, mut group)) in by_key.into_iter().enumerate() {
        group.sort();
        group.dedup();
        units.push(ReviewUnit {
            id: format!("unit-{}", i + 1),
            paths: group,
        });
    }
    units
}

fn unit_key(path: &str) -> String {
    let p = path.replace('\\', "/");
    let parent = Path::new(&p)
        .parent()
        .map(|x| x.to_string_lossy().into_owned())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| ".".into());
    let name = Path::new(&p)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(&p);
    // Collapse message_en.x / message_zh.x into one bundle key.
    for tag in ["_en.", "_zh.", "_ja.", "_ko.", "_ru.", ".en.", ".zh."] {
        if let Some(idx) = name.find(tag) {
            let mut collapsed = name.to_string();
            let repl = if tag.starts_with('.') { "." } else { "." };
            collapsed.replace_range(idx..idx + tag.len() - 1, repl);
            // Simpler: strip locale token
            let collapsed = name.replacen(tag, if tag.starts_with('_') { "." } else { "." }, 1);
            return format!("{parent}/{collapsed}");
        }
    }
    parent
}

fn git(repo: &Path, args: &[&str]) -> anyhow::Result<String> {
    let out = Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .map_err(|e| anyhow::anyhow!("git: {e}"))?;
    if !out.status.success() {
        let err = String::from_utf8_lossy(&out.stderr);
        anyhow::bail!("git {} failed: {}", args.join(" "), err.trim());
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

fn parse_name_status_lines(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        // formats: "M\tpath", "R100\told\tnew", "A\tpath"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            // rename: take new path
            paths.push(parts[parts.len() - 1].to_string());
        } else if parts.len() == 2 {
            paths.push(parts[1].to_string());
        } else if let Some((_, path)) = line.split_once(|c: char| c.is_whitespace()) {
            paths.push(path.trim().to_string());
        }
    }
    paths
}

/// Deterministic changed-file selection. Model never chooses membership.
pub fn select_changed_files(repo: &Path, opts: &ReviewSelectOpts) -> anyhow::Result<Vec<String>> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    match (&opts.from, &opts.to) {
        (Some(from), Some(to)) => {
            crate::git_tools::validate_git_rev(from)?;
            crate::git_tools::validate_git_rev(to)?;
            let out = git(
                repo,
                &["diff", "--name-status", "-z", &format!("{from}...{to}")],
            )
            .or_else(|_| git(repo, &["diff", "--name-status", &format!("{from}...{to}")]))?;
            // Prefer non-null output for simplicity when -z fails on old git
            if out.contains('\0') {
                set.extend(parse_name_status_nul(&out));
            } else {
                set.extend(parse_name_status_lines(&out));
            }
        }
        (Some(from), None) => {
            crate::git_tools::validate_git_rev(from)?;
            let out = git(repo, &["diff", "--name-status", &format!("{from}")])?;
            set.extend(parse_name_status_lines(&out));
        }
        (None, Some(to)) => {
            crate::git_tools::validate_git_rev(to)?;
            let out = git(repo, &["diff", "--name-status", &format!("{to}^...{to}")])?;
            set.extend(parse_name_status_lines(&out));
        }
        (None, None) => {
            // Staged
            let staged = git(repo, &["diff", "--name-status", "--cached"])?;
            set.extend(parse_name_status_lines(&staged));
            // Unstaged
            let unstaged = git(repo, &["diff", "--name-status"])?;
            set.extend(parse_name_status_lines(&unstaged));
            if opts.include_untracked {
                let untracked = git(repo, &["ls-files", "--others", "--exclude-standard"])?;
                for line in untracked.lines() {
                    let p = line.trim();
                    if !p.is_empty() {
                        set.insert(p.to_string());
                    }
                }
            }
        }
    }

    let mut out: Vec<String> = set
        .into_iter()
        .filter(|p| !is_noise_path(p))
        .filter(|p| {
            let full = repo.join(p);
            match std::fs::metadata(&full) {
                Ok(m) => m.is_file() && m.len() <= opts.max_file_bytes,
                // deleted files still reviewable as paths — keep if under size unknown
                Err(_) => true,
            }
        })
        .collect();
    out.sort();
    Ok(out)
}

fn parse_name_status_nul(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut parts = stdout.split('\0').filter(|s| !s.is_empty());
    while let Some(status) = parts.next() {
        if status.starts_with('R') || status.starts_with('C') {
            let _old = parts.next();
            if let Some(new) = parts.next() {
                paths.push(new.to_string());
            }
        } else if let Some(path) = parts.next() {
            paths.push(path.to_string());
        } else if !status.contains('\t') {
            // already path-only
        }
    }
    if paths.is_empty() {
        // fallback: treat as line-oriented
        return parse_name_status_lines(&stdout.replace('\0', "\n"));
    }
    paths
}

/// Lightweight offline heuristics on file contents (no LLM).
pub fn heuristic_findings(repo: &Path, paths: &[String]) -> Vec<Finding> {
    let mut findings = Vec::new();
    for rel in paths {
        let full = repo.join(rel);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        for (i, line) in text.lines().enumerate() {
            let n = (i + 1) as u32;
            let trimmed = line.trim();
            if trimmed.contains("TODO") || trimmed.contains("FIXME") || trimmed.contains("XXX") {
                findings.push(Finding {
                    path: rel.clone(),
                    line: Some(n),
                    severity: FindingSeverity::Nit,
                    kind: "todo".into(),
                    message: format!("unresolved marker: {}", truncate_msg(trimmed, 120)),
                });
            }
            if rel.ends_with(".rs") {
                if trimmed.contains(".unwrap()") && !trimmed.starts_with("//") {
                    findings.push(Finding {
                        path: rel.clone(),
                        line: Some(n),
                        severity: FindingSeverity::Warning,
                        kind: "unwrap".into(),
                        message: "`.unwrap()` may panic; prefer `?` or explicit error handling"
                            .into(),
                    });
                }
                if trimmed.contains("unsafe ") || trimmed.starts_with("unsafe ") {
                    findings.push(Finding {
                        path: rel.clone(),
                        line: Some(n),
                        severity: FindingSeverity::Info,
                        kind: "unsafe".into(),
                        message: "unsafe block/fn — confirm invariants are documented".into(),
                    });
                }
            }
            if (rel.ends_with(".py") || rel.ends_with(".js") || rel.ends_with(".ts"))
                && (trimmed.contains("eval(") || trimmed.contains("exec("))
            {
                findings.push(Finding {
                    path: rel.clone(),
                    line: Some(n),
                    severity: FindingSeverity::Warning,
                    kind: "dynamic_exec".into(),
                    message: "dynamic code execution — verify inputs are not attacker-controlled"
                        .into(),
                });
            }
        }
    }
    filter_precision(findings)
}

fn truncate_msg(s: &str, max: usize) -> String {
    let n = s.chars().count();
    if n <= max {
        s.to_string()
    } else {
        format!("{}…", s.chars().take(max).collect::<String>())
    }
}

/// Build a full offline review report for `repo`.
pub fn run_review(repo: &Path, opts: &ReviewSelectOpts) -> anyhow::Result<ReviewReport> {
    let files = select_changed_files(repo, opts)?;
    let units = partition_units(&files);
    let findings = heuristic_findings(repo, &files);
    let mode = match (&opts.from, &opts.to) {
        (Some(a), Some(b)) => format!("range:{a}...{b}"),
        (Some(a), None) => format!("from:{a}"),
        (None, Some(b)) => format!("to:{b}"),
        (None, None) => "dirty".into(),
    };
    Ok(ReviewReport {
        mode,
        files,
        units,
        findings,
        tool_diet: review_tool_diet(),
    })
}

/// Human + JSON friendly render. Prefer JSON for machines.
pub fn format_report_json(report: &ReviewReport) -> anyhow::Result<String> {
    Ok(serde_json::to_string_pretty(report)?)
}

pub fn format_report_text(report: &ReviewReport) -> String {
    let mut lines = Vec::new();
    lines.push(format!("review mode: {}", report.mode));
    lines.push(format!("files: {}", report.files.len()));
    for f in &report.files {
        lines.push(format!("  - {f}"));
    }
    lines.push(format!("units: {}", report.units.len()));
    for u in &report.units {
        lines.push(format!("  {} ({})", u.id, u.paths.join(", ")));
    }
    lines.push(format!(
        "tool_diet: mutation_allowed={} denied=[{}]",
        report.tool_diet.mutation_allowed,
        report.tool_diet.denied.join(", ")
    ));
    lines.push(format!("findings: {}", report.findings.len()));
    for f in &report.findings {
        let loc = f
            .line
            .map(|l| format!(":{}", l))
            .unwrap_or_default();
        lines.push(format!(
            "  [{}] {}{} ({}) {}",
            f.severity.as_str(),
            f.path,
            loc,
            f.kind,
            f.message
        ));
    }
    if report.findings.is_empty() {
        lines.push("  (none — clean or no heuristic hits)".into());
    }
    lines.join("\n")
}

/// Parse `pirs review` argv tokens after the `review` verb.
pub fn parse_review_cli_args(args: &[&str]) -> ReviewSelectOpts {
    let mut opts = ReviewSelectOpts::dirty_default();
    let mut i = 0;
    while i < args.len() {
        match args[i] {
            "--from" if i + 1 < args.len() => {
                opts.from = Some(args[i + 1].to_string());
                i += 2;
            }
            "--to" if i + 1 < args.len() => {
                opts.to = Some(args[i + 1].to_string());
                i += 2;
            }
            "--no-untracked" => {
                opts.include_untracked = false;
                i += 1;
            }
            "--json" => {
                // handled by caller
                i += 1;
            }
            _ => i += 1,
        }
    }
    opts
}

pub fn wants_json(args: &[&str]) -> bool {
    args.iter().any(|a| *a == "--json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::process::Command;

    fn init_repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        let run = |args: &[&str]| {
            let st = Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap();
            assert!(st.success(), "{args:?}");
        };
        run(&["init"]);
        run(&["config", "user.email", "t@example.com"]);
        run(&["config", "user.name", "t"]);
        std::fs::write(dir.path().join("keep.rs"), "fn a() { 1 }\n").unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "lock\n").unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn ok() {}\n// TODO: finish\npub fn bad() { x.unwrap(); }\n",
        )
        .unwrap();
        run(&["add", "."]);
        run(&["commit", "-m", "init"]);
        dir
    }

    #[test]
    fn noise_filters_lockfiles_and_target() {
        assert!(is_noise_path("Cargo.lock"));
        assert!(is_noise_path("frontend/package-lock.json"));
        assert!(is_noise_path("crates/foo/target/debug/foo"));
        assert!(is_noise_path("app/node_modules/x/index.js"));
        assert!(is_noise_path("assets/logo.png"));
        assert!(!is_noise_path("src/main.rs"));
        assert!(!is_noise_path("crates/pirs-tools/src/code_review.rs"));
    }

    #[test]
    fn select_dirty_excludes_noise_includes_source() {
        let dir = init_repo();
        // dirty edit
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn ok() {}\n// TODO: finish\npub fn bad() { x.unwrap(); }\n// FIXME later\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "lock2\n").unwrap();
        std::fs::write(dir.path().join("new_feature.rs"), "fn n() {}\n").unwrap();

        let files = select_changed_files(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(
            files.iter().any(|f| f.ends_with("lib.rs") || f == "src/lib.rs"),
            "lib.rs missing: {files:?}"
        );
        assert!(
            files.iter().any(|f| f.ends_with("new_feature.rs")),
            "untracked missing: {files:?}"
        );
        assert!(
            !files.iter().any(|f| f.contains("Cargo.lock")),
            "lockfile should be filtered: {files:?}"
        );
    }

    #[test]
    fn partition_groups_same_directory() {
        let paths = vec![
            "src/a.rs".into(),
            "src/b.rs".into(),
            "tests/t.rs".into(),
            "i18n/message_en.properties".into(),
            "i18n/message_zh.properties".into(),
        ];
        let units = partition_units(&paths);
        assert!(units.len() >= 2, "{units:?}");
        // locale pair should share a unit
        let locale = units.iter().find(|u| {
            u.paths.iter().any(|p| p.contains("message_en"))
                && u.paths.iter().any(|p| p.contains("message_zh"))
        });
        assert!(locale.is_some(), "locale bundle missing: {units:?}");
        let src = units.iter().find(|u| {
            u.paths.iter().any(|p| p == "src/a.rs") && u.paths.iter().any(|p| p == "src/b.rs")
        });
        assert!(src.is_some(), "src bundle missing: {units:?}");
    }

    #[test]
    fn precision_filter_drops_unlocated_and_empty() {
        let raw = vec![
            Finding {
                path: "a.rs".into(),
                line: Some(1),
                severity: FindingSeverity::Warning,
                kind: "unwrap".into(),
                message: "bad".into(),
            },
            Finding {
                path: "b.rs".into(),
                line: None,
                severity: FindingSeverity::Info,
                kind: "guess".into(),
                message: "speculative".into(),
            },
            Finding {
                path: "c.rs".into(),
                line: Some(2),
                severity: FindingSeverity::Nit,
                kind: "todo".into(),
                message: "   ".into(),
            },
        ];
        let kept = filter_precision(raw);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].path, "a.rs");
    }

    #[test]
    fn finding_json_roundtrip() {
        let f = Finding {
            path: "x.rs".into(),
            line: Some(10),
            severity: FindingSeverity::Error,
            kind: "bug".into(),
            message: "boom".into(),
        };
        let s = serde_json::to_string(&f).unwrap();
        let back: Finding = serde_json::from_str(&s).unwrap();
        assert_eq!(f, back);
    }

    #[test]
    fn review_diet_denies_mutators() {
        assert!(!review_tool_allowed("write"));
        assert!(!review_tool_allowed("edit"));
        assert!(!review_tool_allowed("ast_edit"));
        assert!(!review_tool_allowed("bash"));
        assert!(!review_tool_allowed("run_tests"));
        assert!(review_tool_allowed("read"));
        assert!(review_tool_allowed("grep"));
        assert!(review_tool_allowed("git"));
        assert!(review_tool_allowed("lsp"));
        let diet = review_tool_diet();
        assert!(!diet.mutation_allowed);
        assert!(diet.denied.iter().any(|d| d == "bash"));
    }

    #[test]
    fn run_review_offline_emits_structured_findings() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn bad() { x.unwrap(); }\n// TODO: fix\n",
        )
        .unwrap();
        let report = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(!report.files.is_empty(), "files: {:?}", report.files);
        assert!(!report.units.is_empty());
        assert!(!report.tool_diet.mutation_allowed);
        // Heuristics should locate unwrap and/or TODO
        assert!(
            !report.findings.is_empty(),
            "expected findings: {:?}",
            report.findings
        );
        for f in &report.findings {
            assert!(f.is_located());
            assert!(!f.is_empty_message());
        }
        let json = format_report_json(&report).unwrap();
        assert!(json.contains("\"findings\""));
        assert!(json.contains("\"tool_diet\""));
    }

    #[test]
    fn select_range_between_commits() {
        let dir = init_repo();
        std::fs::write(dir.path().join("src/extra.rs"), "pub fn e() {}\n").unwrap();
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        };
        run(&["add", "src/extra.rs"]);
        run(&["commit", "-m", "extra"]);
        let files = select_changed_files(
            dir.path(),
            &ReviewSelectOpts::range("HEAD~1", "HEAD"),
        )
        .unwrap();
        assert!(
            files.iter().any(|f| f.contains("extra.rs")),
            "range files: {files:?}"
        );
    }
}
