//! Deterministic high-precision code review prep and offline scan.
//!
//! Selection, noise filtering, unit partitioning, tool diet, and structured
//! findings are pure (or git-only) so tests need no LLM. Heuristics only scan
//! **added** lines from the diff (not whole-file pre-existing noise). Agent
//! review-gate consumes the same plan via the `review_report` host query.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Max file size (bytes) included by default; larger blobs are noise-filtered.
pub const DEFAULT_MAX_FILE_BYTES: u64 = 512 * 1024;

/// Max bytes of unified diff included in a reviewer context blob (precision budget).
pub const DEFAULT_MAX_DIFF_CONTEXT_BYTES: usize = 24_000;

/// Residual risk at or above this value escalates to an LLM subagent (cascade).
pub const LLM_RESIDUAL_RISK_THRESHOLD: u32 = 8;

/// Hysteresis: identical plan fingerprint within this window skips re-review.
pub const HYSTERESIS_MS: u64 = 45_000;

/// Context lines attached to each structured finding (slice).
pub const FINDING_SLICE_RADIUS: usize = 4;

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
    "project",
];

#[derive(Debug, Clone, Default)]
pub struct ReviewSelectOpts {
    /// Compare `from..to` (both optional). Empty = dirty workspace.
    pub from: Option<String>,
    pub to: Option<String>,
    pub include_untracked: bool,
    pub max_file_bytes: u64,
    /// When true (or `PIRS_REVIEW_CARGO=1`), run `cargo check` if `Cargo.toml` exists.
    pub run_cargo_check: bool,
}

impl ReviewSelectOpts {
    pub fn dirty_default() -> Self {
        Self {
            from: None,
            to: None,
            include_untracked: true,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            run_cargo_check: std::env::var("PIRS_REVIEW_CARGO")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }

    pub fn range(from: impl Into<String>, to: impl Into<String>) -> Self {
        Self {
            from: Some(from.into()),
            to: Some(to.into()),
            include_untracked: false,
            max_file_bytes: DEFAULT_MAX_FILE_BYTES,
            run_cargo_check: std::env::var("PIRS_REVIEW_CARGO")
                .map(|v| matches!(v.as_str(), "1" | "true" | "yes"))
                .unwrap_or(false),
        }
    }
}

/// Gate action for review-gate / hosts (pure; no Rhai).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GateAction {
    /// No files / nothing to do.
    PassEmpty,
    /// Structured error findings — block without LLM.
    AutoBlock,
    /// Same plan fingerprint recently reviewed.
    SkipHysteresis,
    /// Residual risk below threshold — skip expensive LLM.
    SkipLlmLowRisk,
    /// Run adversarial LLM subagent.
    NeedsLlm,
}

/// Decide what the session gate should do with a finished report.
pub fn decide_gate_action(report: &ReviewReport) -> GateAction {
    if report.files.is_empty() {
        return GateAction::PassEmpty;
    }
    if report.findings.iter().any(|f| f.is_blocking()) {
        return GateAction::AutoBlock;
    }
    if report.rubric.hysteresis_skip {
        return GateAction::SkipHysteresis;
    }
    if report.rubric.needs_llm {
        GateAction::NeedsLlm
    } else {
        GateAction::SkipLlmLowRisk
    }
}

/// Merge two independent LLM verdicts: CRITICAL only if **both** say CRITICAL
/// (self-consistency for precision).
pub fn merge_llm_verdicts(a: &str, b: &str) -> &'static str {
    let a_c = first_verdict_line(a).starts_with("CRITICAL");
    let b_c = first_verdict_line(b).starts_with("CRITICAL");
    if a_c && b_c {
        "CRITICAL"
    } else {
        "SOUND"
    }
}

fn first_verdict_line(s: &str) -> String {
    s.lines()
        .map(|l| l.trim().trim_start_matches('#').trim().trim_matches('*').to_ascii_uppercase())
        .find(|l| !l.is_empty())
        .unwrap_or_default()
}

/// Stable key for dismiss / FP memory.
pub fn finding_dismiss_key(f: &Finding) -> String {
    format!(
        "{}|{}|{}|{}",
        f.kind,
        f.path,
        f.line.unwrap_or(0),
        f.message.chars().take(80).collect::<String>()
    )
}

/// Load dismissed finding keys from `{repo}/.pirs/review-dismissed.json`.
pub fn load_dismissed_keys(repo: &Path) -> BTreeSet<String> {
    let path = repo.join(".pirs").join("review-dismissed.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return BTreeSet::new();
    };
    serde_json::from_str::<Vec<String>>(&text)
        .unwrap_or_default()
        .into_iter()
        .collect()
}

/// Drop findings whose dismiss key is in the set.
pub fn filter_dismissed(findings: Vec<Finding>, dismissed: &BTreeSet<String>) -> Vec<Finding> {
    findings
        .into_iter()
        .filter(|f| !dismissed.contains(&finding_dismiss_key(f)))
        .collect()
}

/// One isolated review unit (related paths share a unit).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReviewUnit {
    pub id: String,
    pub paths: Vec<String>,
    /// Higher = review sooner / more LLM attention (deterministic).
    #[serde(default)]
    pub risk_score: u32,
    /// Hash of unit file contents (for skip-cache / incremental review).
    #[serde(default)]
    pub content_hash: String,
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
    /// Local source slice around `line` (causal context, not whole file).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slice: Option<String>,
}

impl Finding {
    pub fn is_located(&self) -> bool {
        self.line.is_some()
    }

    pub fn is_empty_message(&self) -> bool {
        self.message.trim().is_empty()
    }

    pub fn is_blocking(&self) -> bool {
        matches!(self.severity, FindingSeverity::Error)
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
    /// Deterministic cascade / hysteresis signals for the gate.
    #[serde(default)]
    pub rubric: ReviewRubric,
}

/// Compact rubric + cascade decision (no freeform prose required).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct ReviewRubric {
    /// 0–3 style axes derived from heuristics (not LLM).
    pub correctness: u8,
    pub security: u8,
    pub test_gaming: u8,
    /// Combined residual risk for cascade thresholding.
    pub residual_risk: u32,
    /// True when residual risk warrants an LLM subagent.
    pub needs_llm: bool,
    /// Stable fingerprint of this plan (files + unit hashes + finding kinds).
    pub plan_fingerprint: String,
    /// Units skipped because content hash matched a recent review.
    pub skipped_cached_units: usize,
    /// True when the same fingerprint was reviewed recently (hysteresis).
    pub hysteresis_skip: bool,
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
    // Top-level noise dirs
    if lower == "target"
        || lower.starts_with("target/")
        || lower == "node_modules"
        || lower.starts_with("node_modules/")
    {
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

/// Reject paths that try to escape the repo via `..` or absolute form.
pub fn is_safe_repo_rel_path(path: &str) -> bool {
    let p = path.replace('\\', "/");
    if p.is_empty() || p.starts_with('/') || p.starts_with("~/") {
        return false;
    }
    if p.split('/').any(|c| c == "..") {
        return false;
    }
    true
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
    // Allowlist-only (plus lsp* namespace).
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
/// Call [`enrich_units`] afterward to attach risk scores and content hashes.
pub fn partition_units(paths: &[String]) -> Vec<ReviewUnit> {
    if paths.is_empty() {
        return Vec::new();
    }
    let mut by_key: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for p in paths {
        by_key.entry(unit_key(p)).or_default().push(p.clone());
    }
    let mut units = Vec::new();
    for (i, (_k, mut group)) in by_key.into_iter().enumerate() {
        group.sort();
        group.dedup();
        units.push(ReviewUnit {
            id: format!("unit-{}", i + 1),
            paths: group,
            risk_score: 0,
            content_hash: String::new(),
        });
    }
    units
}

/// Attach content hashes + risk scores and sort units highest-risk first.
pub fn enrich_units(repo: &Path, units: &mut [ReviewUnit]) {
    for u in units.iter_mut() {
        u.content_hash = unit_content_hash(repo, &u.paths);
        u.risk_score = unit_risk_score(repo, &u.paths);
    }
    units.sort_by(|a, b| {
        b.risk_score
            .cmp(&a.risk_score)
            .then_with(|| a.id.cmp(&b.id))
    });
    // Re-id for stable display after sort
    for (i, u) in units.iter_mut().enumerate() {
        u.id = format!("unit-{}", i + 1);
    }
}

/// FNV-1a over path list + file bytes (or "MISSING") — fast, no extra deps.
pub fn unit_content_hash(repo: &Path, paths: &[String]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for p in paths {
        for b in p.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= 0xff;
        h = h.wrapping_mul(0x100000001b3);
        let full = repo.join(p);
        match std::fs::read(&full) {
            Ok(bytes) => {
                for b in bytes {
                    h ^= u64::from(b);
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
            Err(_) => {
                for b in b"MISSING" {
                    h ^= u64::from(*b);
                    h = h.wrapping_mul(0x100000001b3);
                }
            }
        }
    }
    format!("{h:016x}")
}

/// Deterministic risk: surface path + cheap content cues + size.
pub fn unit_risk_score(repo: &Path, paths: &[String]) -> u32 {
    let mut score: u32 = 0;
    for p in paths {
        let lower = p.replace('\\', "/").to_ascii_lowercase();
        score = score.saturating_add(1); // each path
        if is_likely_test_path(p) {
            // tests matter for gaming but lower default risk than prod
            score = score.saturating_add(1);
        } else {
            score = score.saturating_add(2);
        }
        for needle in [
            "auth", "crypto", "password", "secret", "token", "oauth", "login", "session", "unsafe",
            "ffi", "socket", "http", "tls", "ssh", "sandbox", "permission", "path", "exec",
        ] {
            if lower.contains(needle) {
                score = score.saturating_add(4);
            }
        }
        let full = repo.join(p);
        if let Ok(text) = std::fs::read_to_string(&full) {
            let n = text.len() as u32;
            score = score.saturating_add((n / 2048).min(8));
            if text.contains("unsafe ") {
                score = score.saturating_add(6);
            }
            if text.contains(".unwrap()") && !is_likely_test_path(p) {
                score = score.saturating_add(3);
            }
            if text.contains("eval(") || text.contains("exec(") {
                score = score.saturating_add(5);
            }
        }
    }
    score
}

/// Process-local unit content-hash cache (skip unchanged units across gate fires).
fn unit_hash_cache() -> &'static std::sync::Mutex<BTreeMap<String, u64>> {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, u64>>> =
        std::sync::OnceLock::new();
    CACHE.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Per-fingerprint last-seen times so concurrent reviews/tests don't stomp each other.
fn last_plan_map() -> &'static std::sync::Mutex<BTreeMap<String, std::time::Instant>> {
    static MAP: std::sync::OnceLock<std::sync::Mutex<BTreeMap<String, std::time::Instant>>> =
        std::sync::OnceLock::new();
    MAP.get_or_init(|| std::sync::Mutex::new(BTreeMap::new()))
}

/// Mark unit hashes as reviewed now; returns how many were already cached (unchanged).
pub fn apply_unit_cache(units: &[ReviewUnit]) -> usize {
    let now = now_millis();
    let mut cache = unit_hash_cache().lock().unwrap();
    let mut skipped = 0usize;
    for u in units {
        if u.content_hash.is_empty() {
            continue;
        }
        if cache.contains_key(&u.content_hash) {
            skipped += 1;
        }
        cache.insert(u.content_hash.clone(), now);
    }
    // Cap cache size
    if cache.len() > 512 {
        let mut keys: Vec<(u64, String)> = cache
            .iter()
            .map(|(k, t)| (*t, k.clone()))
            .collect();
        keys.sort_by_key(|(t, _)| *t);
        for (_, k) in keys.into_iter().take(cache.len().saturating_sub(256)) {
            cache.remove(&k);
        }
    }
    skipped
}

fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Fingerprint of plan identity for hysteresis.
pub fn plan_fingerprint(files: &[String], units: &[ReviewUnit], findings: &[Finding]) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for f in files {
        for b in f.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
    }
    for u in units {
        for b in u.content_hash.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= u64::from(u.risk_score);
        h = h.wrapping_mul(0x100000001b3);
    }
    for f in findings {
        for b in f.kind.as_bytes() {
            h ^= u64::from(*b);
            h = h.wrapping_mul(0x100000001b3);
        }
        h ^= u64::from(f.line.unwrap_or(0));
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// True if this fingerprint was seen within `window_ms`.
pub fn hysteresis_should_skip(fingerprint: &str, window_ms: u64) -> bool {
    if fingerprint.is_empty() {
        return false;
    }
    let mut map = last_plan_map().lock().unwrap();
    let now = std::time::Instant::now();
    // Drop expired entries
    map.retain(|_, t| t.elapsed().as_millis() as u64 <= window_ms.saturating_mul(2));
    if let Some(t0) = map.get(fingerprint) {
        if t0.elapsed().as_millis() as u64 <= window_ms {
            return true;
        }
    }
    map.insert(fingerprint.to_string(), now);
    if map.len() > 256 {
        // drop oldest
        let mut pairs: Vec<_> = map.iter().map(|(k, t)| (*t, k.clone())).collect();
        pairs.sort_by_key(|(t, _)| *t);
        for (_, k) in pairs.into_iter().take(map.len().saturating_sub(128)) {
            map.remove(&k);
        }
    }
    false
}

/// Build rubric + cascade decision from findings and unit risks.
pub fn build_rubric(
    files: &[String],
    units: &[ReviewUnit],
    findings: &[Finding],
    skipped_cached: usize,
) -> ReviewRubric {
    let mut correctness = 0u8;
    let mut security = 0u8;
    let mut test_gaming = 0u8;
    for f in findings {
        match f.kind.as_str() {
            "unwrap" | "panic" | "todo_macro" => {
                correctness = correctness.saturating_add(1).min(3)
            }
            "todo" => {}
            "unsafe" | "dynamic_exec" | "shell_injection" | "secret" => {
                security = security.saturating_add(2).min(3)
            }
            "test_gaming" => test_gaming = test_gaming.saturating_add(3).min(3),
            k if k.starts_with("cargo_error") => correctness = 3,
            k if k.starts_with("cargo_") => correctness = correctness.saturating_add(1).min(3),
            _ => {}
        }
        match f.severity {
            FindingSeverity::Error => {
                if f.kind == "secret" || f.kind == "shell_injection" {
                    security = 3;
                } else {
                    correctness = 3;
                }
            }
            FindingSeverity::Warning => {
                correctness = correctness.saturating_add(1).min(3);
            }
            _ => {}
        }
    }
    let unit_risk: u32 = units.iter().map(|u| u.risk_score).sum();
    let top = units.first().map(|u| u.risk_score).unwrap_or(0);
    let finding_risk: u32 = findings
        .iter()
        .map(|f| match f.severity {
            FindingSeverity::Error => 12,
            FindingSeverity::Warning => 5,
            FindingSeverity::Info => 2,
            FindingSeverity::Nit => 1,
        })
        .sum();
    // Cached units reduce residual (already reviewed content).
    let cache_relief = (skipped_cached as u32).saturating_mul(2);
    let residual_risk = unit_risk
        .saturating_add(finding_risk)
        .saturating_add(top / 2)
        .saturating_sub(cache_relief);
    let fp = plan_fingerprint(files, units, findings);
    let hysteresis_skip = hysteresis_should_skip(&fp, HYSTERESIS_MS);
    let needs_llm = !hysteresis_skip
        && (residual_risk >= LLM_RESIDUAL_RISK_THRESHOLD
            || findings.iter().any(|f| f.is_blocking())
            || security >= 2);
    ReviewRubric {
        correctness,
        security,
        test_gaming,
        residual_risk,
        needs_llm,
        plan_fingerprint: fp,
        skipped_cached_units: skipped_cached,
        hysteresis_skip,
    }
}

/// Attach source slices around finding lines.
pub fn attach_finding_slices(repo: &Path, findings: &mut [Finding]) {
    for f in findings.iter_mut() {
        let Some(line) = f.line else {
            continue;
        };
        let full = repo.join(&f.path);
        let Ok(text) = std::fs::read_to_string(&full) else {
            continue;
        };
        let lines: Vec<&str> = text.lines().collect();
        if lines.is_empty() {
            continue;
        }
        let idx = (line as usize).saturating_sub(1);
        if idx >= lines.len() {
            continue;
        }
        let start = idx.saturating_sub(FINDING_SLICE_RADIUS);
        let end = (idx + FINDING_SLICE_RADIUS + 1).min(lines.len());
        let mut slice = String::new();
        for (i, l) in lines[start..end].iter().enumerate() {
            let n = start + i + 1;
            let mark = if n == line as usize { '>' } else { ' ' };
            slice.push_str(&format!("{mark}{n:4}| {l}\n"));
        }
        f.slice = Some(slice);
    }
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
        if name.contains(tag) {
            let collapsed = name.replacen(tag, ".", 1);
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

/// Parse `git diff --name-status` (newline / tab form).
fn parse_name_status_lines(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    for line in stdout.lines() {
        let line = line.trim_end();
        if line.is_empty() {
            continue;
        }
        // "M\tpath", "R100\told\tnew", "A\tpath"
        let parts: Vec<&str> = line.split('\t').collect();
        if parts.len() >= 3 {
            paths.push(parts[parts.len() - 1].to_string());
        } else if parts.len() == 2 {
            paths.push(parts[1].to_string());
        } else if let Some((_, path)) = line.split_once(|c: char| c.is_whitespace()) {
            paths.push(path.trim().to_string());
        }
    }
    paths
}

/// Parse `git diff --name-status -z` (NUL field terminators).
fn parse_name_status_nul(stdout: &str) -> Vec<String> {
    let mut paths = Vec::new();
    let mut parts = stdout.split('\0').filter(|s| !s.is_empty()).peekable();
    while let Some(status) = parts.next() {
        // Status may be "M", "A", "D", "R100", "C100", …
        if status.starts_with('R') || status.starts_with('C') {
            let _old = parts.next();
            if let Some(new) = parts.next() {
                paths.push(new.to_string());
            }
        } else if looks_like_status(status) {
            if let Some(path) = parts.next() {
                paths.push(path.to_string());
            }
        } else {
            // Unexpected: treat token as path (defensive).
            paths.push(status.to_string());
        }
    }
    paths
}

fn looks_like_status(s: &str) -> bool {
    let b = s.as_bytes();
    if b.is_empty() {
        return false;
    }
    matches!(b[0], b'M' | b'A' | b'D' | b'T' | b'U' | b'R' | b'C' | b'X' | b'B')
        && b.iter().skip(1).all(|c| c.is_ascii_digit() || *c == b' ')
}

fn name_status(repo: &Path, extra: &[&str]) -> anyhow::Result<Vec<String>> {
    let mut args = vec!["diff", "--name-status", "-z"];
    args.extend_from_slice(extra);
    match git(repo, &args) {
        Ok(out) if !out.is_empty() => Ok(parse_name_status_nul(&out)),
        Ok(_) => Ok(Vec::new()),
        Err(_) => {
            // Older git / odd envs: drop -z
            let mut args = vec!["diff", "--name-status"];
            args.extend_from_slice(extra);
            let out = git(repo, &args)?;
            Ok(parse_name_status_lines(&out))
        }
    }
}

/// Deterministic changed-file selection. Model never chooses membership.
pub fn select_changed_files(repo: &Path, opts: &ReviewSelectOpts) -> anyhow::Result<Vec<String>> {
    let mut set: BTreeSet<String> = BTreeSet::new();

    match (&opts.from, &opts.to) {
        (Some(from), Some(to)) => {
            crate::git_tools::validate_git_rev(from)?;
            crate::git_tools::validate_git_rev(to)?;
            let range = format!("{from}...{to}");
            set.extend(name_status(repo, &[&range])?);
        }
        (Some(from), None) => {
            crate::git_tools::validate_git_rev(from)?;
            // Working tree + index vs from
            set.extend(name_status(repo, &[from.as_str()])?);
        }
        (None, Some(to)) => {
            crate::git_tools::validate_git_rev(to)?;
            let range = format!("{to}^...{to}");
            set.extend(name_status(repo, &[&range])?);
        }
        (None, None) => {
            set.extend(name_status(repo, &["--cached"])?);
            set.extend(name_status(repo, &[])?);
            if opts.include_untracked {
                let untracked = git(repo, &["ls-files", "--others", "--exclude-standard", "-z"])?;
                if untracked.contains('\0') {
                    for p in untracked.split('\0') {
                        if !p.is_empty() {
                            set.insert(p.to_string());
                        }
                    }
                } else {
                    for line in untracked.lines() {
                        let p = line.trim();
                        if !p.is_empty() {
                            set.insert(p.to_string());
                        }
                    }
                }
            }
        }
    }

    let mut out: Vec<String> = set
        .into_iter()
        .filter(|p| is_safe_repo_rel_path(p))
        .filter(|p| !is_noise_path(p))
        .filter(|p| {
            let full = repo.join(p);
            match std::fs::metadata(&full) {
                Ok(m) => m.is_file() && m.len() <= opts.max_file_bytes,
                // Deleted path still listed for review of the deletion.
                Err(_) => true,
            }
        })
        .collect();
    out.sort();
    Ok(out)
}

/// Collect (path, new_line_no, line_text) for **added** lines only (`+` hunks).
pub fn added_lines_from_diff(repo: &Path, opts: &ReviewSelectOpts) -> anyhow::Result<Vec<(String, u32, String)>> {
    let mut args = vec!["diff", "-U0", "--no-color", "--no-ext-diff"];
    let range_owned: String;
    match (&opts.from, &opts.to) {
        (Some(from), Some(to)) => {
            range_owned = format!("{from}...{to}");
            args.push(&range_owned);
        }
        (Some(from), None) => args.push(from.as_str()),
        (None, Some(to)) => {
            range_owned = format!("{to}^...{to}");
            args.push(&range_owned);
        }
        (None, None) => {
            // Staged then unstaged: two diffs
            let mut all = Vec::new();
            all.extend(parse_unified_added(
                &git(repo, &["diff", "-U0", "--no-color", "--cached"])?,
            ));
            all.extend(parse_unified_added(
                &git(repo, &["diff", "-U0", "--no-color"])?,
            ));
            // Untracked: treat every line as added at its line number
            if opts.include_untracked {
                let files = select_changed_files(repo, opts)?;
                for rel in files {
                    let full = repo.join(&rel);
                    if let Ok(text) = std::fs::read_to_string(&full) {
                        // Only pure untracked: not in HEAD
                        let in_index = git(repo, &["ls-files", "--error-unmatch", &rel]).is_ok();
                        if !in_index {
                            for (i, line) in text.lines().enumerate() {
                                all.push((rel.clone(), (i + 1) as u32, line.to_string()));
                            }
                        }
                    }
                }
            }
            return Ok(all);
        }
    }
    let out = git(repo, &args)?;
    Ok(parse_unified_added(&out))
}

fn parse_unified_added(diff: &str) -> Vec<(String, u32, String)> {
    let mut out = Vec::new();
    let mut path = String::new();
    let mut new_line: u32 = 0;
    for line in diff.lines() {
        if let Some(rest) = line.strip_prefix("+++ b/") {
            path = rest.to_string();
            continue;
        }
        if line.starts_with("+++ /dev/null") {
            path.clear();
            continue;
        }
        if let Some(rest) = line.strip_prefix("@@ ") {
            // @@ -a,b +c,d @@
            if let Some(plus) = rest.split_whitespace().find(|t| t.starts_with('+')) {
                let num = plus.trim_start_matches('+').split(',').next().unwrap_or("0");
                new_line = num.parse().unwrap_or(0);
                // When +0,0 (file deleted) stay 0
                if new_line == 0 {
                    // next + line will still increment from 0 — skip increments carefully
                }
            }
            continue;
        }
        if path.is_empty() {
            continue;
        }
        if let Some(body) = line.strip_prefix('+') {
            if line.starts_with("+++") {
                continue;
            }
            if new_line > 0 {
                out.push((path.clone(), new_line, body.to_string()));
            }
            if new_line > 0 {
                new_line += 1;
            }
        } else if line.starts_with('-') {
            // removed line: do not advance new_line
        } else if line.starts_with(' ') {
            if new_line > 0 {
                new_line += 1;
            }
        }
    }
    out
}

/// Lightweight offline heuristics on **added** lines only (no LLM).
pub fn heuristic_findings(repo: &Path, opts: &ReviewSelectOpts) -> anyhow::Result<Vec<Finding>> {
    let added = added_lines_from_diff(repo, opts)?;
    let mut findings = Vec::new();
    for (rel, n, line) in added {
        if is_noise_path(&rel) || !is_safe_repo_rel_path(&rel) {
            continue;
        }
        findings.extend(scan_added_line(&rel, n, &line));
    }
    Ok(filter_precision(findings))
}

/// Scan one added line for structured findings (unit-testable pure logic).
pub fn scan_added_line(rel: &str, n: u32, line: &str) -> Vec<Finding> {
    let mut findings = Vec::new();
    let trimmed = line.trim();
    if trimmed.is_empty() || trimmed.starts_with("//") || trimmed.starts_with('#') {
        // still allow TODO in comments
        if contains_todo_marker(trimmed) {
            findings.push(finding(
                rel,
                n,
                FindingSeverity::Nit,
                "todo",
                format!("unresolved marker: {}", truncate_msg(trimmed, 120)),
            ));
        }
        return findings;
    }

    if contains_todo_marker(trimmed) {
        findings.push(finding(
            rel,
            n,
            FindingSeverity::Nit,
            "todo",
            format!("unresolved marker: {}", truncate_msg(trimmed, 120)),
        ));
    }

    if let Some(msg) = looks_like_hardcoded_secret(trimmed) {
        findings.push(finding(rel, n, FindingSeverity::Error, "secret", msg));
    }

    if looks_like_shell_injection(trimmed) {
        findings.push(finding(
            rel,
            n,
            FindingSeverity::Error,
            "shell_injection",
            "shell invocation with string command — prefer argv arrays, no `sh -c`".into(),
        ));
    }

    if looks_like_panic(trimmed) {
        findings.push(finding(
            rel,
            n,
            FindingSeverity::Warning,
            "panic",
            "explicit panic — prefer Result / recoverable error for library code".into(),
        ));
    }

    if is_script_path(rel)
        && (trimmed.contains("eval(") || trimmed.contains("exec(") || trimmed.contains("Function("))
    {
        findings.push(finding(
            rel,
            n,
            FindingSeverity::Warning,
            "dynamic_exec",
            "dynamic code execution — verify inputs are not attacker-controlled".into(),
        ));
    }

    if (rel.ends_with(".rs") || rel.ends_with(".go")) && !is_likely_test_path(rel) {
        if trimmed.contains(".unwrap()") || trimmed.contains(".expect(") {
            findings.push(finding(
                rel,
                n,
                FindingSeverity::Warning,
                "unwrap",
                "`.unwrap()`/`.expect()` may panic; prefer `?` or explicit error handling".into(),
            ));
        }
        if trimmed.contains("unsafe ") || trimmed.starts_with("unsafe ") {
            findings.push(finding(
                rel,
                n,
                FindingSeverity::Info,
                "unsafe",
                "unsafe block/fn — confirm invariants are documented".into(),
            ));
        }
        if trimmed.contains("todo!(") || trimmed.contains("unimplemented!(") {
            findings.push(finding(
                rel,
                n,
                FindingSeverity::Warning,
                "todo_macro",
                "todo!/unimplemented! will panic if hit at runtime".into(),
            ));
        }
    }

    findings
}

fn finding(path: &str, line: u32, sev: FindingSeverity, kind: &str, message: String) -> Finding {
    Finding {
        path: path.to_string(),
        line: Some(line),
        severity: sev,
        kind: kind.into(),
        message,
        slice: None,
    }
}

fn looks_like_hardcoded_secret(line: &str) -> Option<String> {
    let lower = line.to_ascii_lowercase();
    // assignment-ish secrets in source
    let keys = [
        "password",
        "passwd",
        "secret",
        "api_key",
        "apikey",
        "access_key",
        "private_key",
        "auth_token",
        "client_secret",
    ];
    let has_key = keys.iter().any(|k| lower.contains(k));
    if !has_key {
        // sk-… style tokens
        if line.contains("sk-") && line.chars().filter(|c| c.is_ascii_alphanumeric() || *c == '-').count() > 20
        {
            return Some("possible API key literal in source".into());
        }
        return None;
    }
    // string literal on the line
    if line.contains('"') || line.contains('\'') {
        // skip obvious placeholders
        if lower.contains("env")
            || lower.contains("getenv")
            || lower.contains("std::env")
            || lower.contains("placeholder")
            || lower.contains("example")
            || lower.contains("changeme")
            || lower.contains("***")
        {
            return None;
        }
        return Some(format!(
            "possible hardcoded secret: {}",
            truncate_msg(line.trim(), 100)
        ));
    }
    None
}

fn looks_like_shell_injection(line: &str) -> bool {
    let l = line.replace(' ', "");
    line.contains("sh\").arg(\"-c\")")
        || line.contains("sh').arg('-c')")
        || line.contains("Command::new(\"sh\")")
        || line.contains("Command::new(\"bash\")")
        || line.contains("Command::new(\"cmd\")")
        || line.contains("std::process::Command::new(\"sh\")")
        || line.contains("shell=True")
        || line.contains("subprocess.call(") && line.contains("shell=True")
        || line.contains("os.system(")
        || l.contains("new(\"sh\")") && line.contains("-c")
        || line.contains("bash -c")
        || line.contains("sh -c")
}

fn looks_like_panic(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with("panic!(")
        || t.contains(" panic!(")
        || t.starts_with("unreachable!(")
        || (t.contains("assert!(") && t.contains("false"))
}

/// Optional `cargo check` diagnostics as structured findings (Rust workspaces).
pub fn cargo_check_findings(repo: &Path) -> Vec<Finding> {
    if !repo.join("Cargo.toml").exists() {
        return Vec::new();
    }
    let out = Command::new("cargo")
        .args(["check", "--message-format=json", "--quiet"])
        .current_dir(repo)
        .output();
    let Ok(out) = out else {
        return Vec::new();
    };
    let mut findings = Vec::new();
    let stdout = String::from_utf8_lossy(&out.stdout);
    for line in stdout.lines() {
        let Ok(v) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };
        if v.get("reason").and_then(|r| r.as_str()) != Some("compiler-message") {
            continue;
        }
        let msg = match v.get("message") {
            Some(m) => m,
            None => continue,
        };
        let level = msg.get("level").and_then(|l| l.as_str()).unwrap_or("");
        if level != "error" && level != "warning" {
            continue;
        }
        let message = msg
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .to_string();
        let spans = msg
            .get("spans")
            .and_then(|s| s.as_array())
            .cloned()
            .unwrap_or_default();
        let primary = spans.iter().find(|s| {
            s.get("is_primary")
                .and_then(|p| p.as_bool())
                .unwrap_or(false)
        });
        let (path, line_no) = if let Some(sp) = primary {
            let p = sp
                .get("file_name")
                .and_then(|f| f.as_str())
                .unwrap_or("")
                .to_string();
            let ln = sp.get("line_start").and_then(|l| l.as_u64()).unwrap_or(0) as u32;
            (p, ln)
        } else {
            (String::new(), 0)
        };
        if path.is_empty() || line_no == 0 {
            continue;
        }
        // Prefer repo-relative paths
        let path = path
            .strip_prefix(&format!("{}/", repo.display()))
            .unwrap_or(&path)
            .to_string();
        let sev = if level == "error" {
            FindingSeverity::Error
        } else {
            FindingSeverity::Warning
        };
        findings.push(Finding {
            path,
            line: Some(line_no),
            severity: sev,
            kind: format!("cargo_{level}"),
            message: truncate_msg(&message, 200),
            slice: None,
        });
    }
    filter_precision(findings)
}

fn contains_todo_marker(s: &str) -> bool {
    // Word-ish markers only — avoid matching random "XXX" hex noise alone without TODO/FIXME.
    s.contains("TODO") || s.contains("FIXME") || s.contains("TODO:") || s.contains("FIXME:")
}

fn is_likely_test_path(path: &str) -> bool {
    let p = path.replace('\\', "/").to_ascii_lowercase();
    p.contains("/tests/")
        || p.contains("/test/")
        || p.ends_with("_test.rs")
        || p.ends_with("_tests.rs")
        || p.ends_with(".test.ts")
        || p.ends_with(".test.js")
        || p.ends_with("_test.py")
        || p.starts_with("tests/")
}

fn is_script_path(path: &str) -> bool {
    path.ends_with(".py")
        || path.ends_with(".js")
        || path.ends_with(".ts")
        || path.ends_with(".jsx")
        || path.ends_with(".tsx")
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
    let mut units = partition_units(&files);
    enrich_units(repo, &mut units);
    let skipped_cached = apply_unit_cache(&units);
    let mut findings = heuristic_findings(repo, opts)?;
    if opts.run_cargo_check {
        findings.extend(cargo_check_findings(repo));
    }
    let dismissed = load_dismissed_keys(repo);
    findings = filter_dismissed(findings, &dismissed);
    attach_finding_slices(repo, &mut findings);
    let rubric = build_rubric(&files, &units, &findings, skipped_cached);
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
        rubric,
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
    lines.push(format!("units: {} (risk-ranked)", report.units.len()));
    for u in &report.units {
        lines.push(format!(
            "  {} risk={} hash={} ({})",
            u.id,
            u.risk_score,
            &u.content_hash[..u.content_hash.len().min(8)],
            u.paths.join(", ")
        ));
    }
    lines.push(format!(
        "tool_diet: mutation_allowed={} denied=[{}]",
        report.tool_diet.mutation_allowed,
        report.tool_diet.denied.join(", ")
    ));
    let r = &report.rubric;
    lines.push(format!(
        "rubric: residual_risk={} needs_llm={} hysteresis_skip={} cached_units={} \
         correctness={} security={} test_gaming={} fp={}",
        r.residual_risk,
        r.needs_llm,
        r.hysteresis_skip,
        r.skipped_cached_units,
        r.correctness,
        r.security,
        r.test_gaming,
        r.plan_fingerprint
    ));
    if r.hysteresis_skip {
        lines.push("HYSTERESIS:skip".into());
    }
    if r.needs_llm {
        lines.push(format!("CASCADE:needs_llm residual={}", r.residual_risk));
    } else {
        lines.push(format!("CASCADE:skip_llm residual={}", r.residual_risk));
    }
    lines.push(format!("findings: {}", report.findings.len()));
    for f in &report.findings {
        let loc = f.line.map(|l| format!(":{l}")).unwrap_or_default();
        lines.push(format!(
            "  [{}] {}{} ({}) {}",
            f.severity.as_str(),
            f.path,
            loc,
            f.kind,
            f.message
        ));
        if let Some(slice) = &f.slice {
            lines.push("  --- slice ---".into());
            for sl in slice.lines() {
                lines.push(format!("  {sl}"));
            }
        }
    }
    if report.findings.is_empty() {
        lines.push("  (none — clean or no heuristic hits)".into());
    }
    lines.join("\n")
}

/// Context blob for an independent reviewer subagent (gate / CLI).
/// Includes file list, units, structured findings, diet, and a capped unified diff.
pub fn format_reviewer_context(
    repo: &Path,
    report: &ReviewReport,
    opts: &ReviewSelectOpts,
    max_diff_bytes: usize,
) -> String {
    let mut parts = Vec::new();
    parts.push(format_report_text(report));
    parts.push(String::new());
    parts.push("## tool diet (hard constraints)".into());
    parts.push(format!(
        "ALLOWED: {}",
        report.tool_diet.allowed.join(", ")
    ));
    parts.push(format!("DENIED: {}", report.tool_diet.denied.join(", ")));
    parts.push(
        "You MUST NOT request or assume write/edit/bash/ast_edit. Observation only.".into(),
    );
    if report.findings.iter().any(|f| f.is_blocking()) {
        parts.push(String::new());
        parts.push("AUTO_BLOCK: structured error-severity findings present.".into());
    }
    // Diff only top-risk units (foraging under budget), not the entire file list.
    let top_paths: Vec<String> = report
        .units
        .iter()
        .take(3)
        .flat_map(|u| u.paths.iter().cloned())
        .collect();
    let diff_paths = if top_paths.is_empty() {
        report.files.clone()
    } else {
        top_paths
    };
    parts.push(String::new());
    parts.push("## full files (top-risk units, capped each)".into());
    const MAX_FILE_CHARS: usize = 8_000;
    for p in &diff_paths {
        let full = repo.join(p);
        match std::fs::read_to_string(&full) {
            Ok(body) => {
                let body = if body.chars().count() > MAX_FILE_CHARS {
                    format!(
                        "{}…\n[truncated at {MAX_FILE_CHARS} chars]",
                        body.chars().take(MAX_FILE_CHARS).collect::<String>()
                    )
                } else {
                    body
                };
                parts.push(format!("### file: {p}\n```\n{body}\n```"));
            }
            Err(_) => parts.push(format!("### file: {p}\n(missing or deleted)")),
        }
    }
    parts.push(String::new());
    parts.push(format!(
        "## unified diff (capped, top-risk units only, {} files)",
        diff_paths.len()
    ));
    let diff = collect_diff_text(repo, opts, &diff_paths).unwrap_or_default();
    let capped = if diff.len() > max_diff_bytes {
        let mut end = max_diff_bytes;
        while end > 0 && !diff.is_char_boundary(end) {
            end -= 1;
        }
        format!(
            "{}\n\n… [diff truncated at {max_diff_bytes} bytes; plan has {} files]",
            &diff[..end],
            report.files.len()
        )
    } else if diff.is_empty() {
        "(no textual diff — untracked-only or binary)".into()
    } else {
        diff
    };
    parts.push(capped);
    parts.push(String::new());
    parts.push(format!("## gate_action: {:?}", decide_gate_action(report)));
    parts.join("\n")
}

fn collect_diff_text(
    repo: &Path,
    opts: &ReviewSelectOpts,
    files: &[String],
) -> anyhow::Result<String> {
    if files.is_empty() {
        return Ok(String::new());
    }
    let mut args: Vec<String> = vec![
        "diff".into(),
        "--no-color".into(),
        "--no-ext-diff".into(),
        "-U3".into(),
    ];
    match (&opts.from, &opts.to) {
        (Some(a), Some(b)) => args.push(format!("{a}...{b}")),
        (Some(a), None) => args.push(a.clone()),
        (None, Some(b)) => args.push(format!("{b}^...{b}")),
        (None, None) => {
            // staged + unstaged for selected paths
            let mut out = String::new();
            let mut a1: Vec<String> = vec![
                "diff".into(),
                "--cached".into(),
                "--no-color".into(),
                "-U3".into(),
                "--".into(),
            ];
            a1.extend(files.iter().cloned());
            let s1: Vec<&str> = a1.iter().map(|s| s.as_str()).collect();
            if let Ok(s) = git(repo, &s1) {
                out.push_str(&s);
            }
            let mut a2: Vec<String> =
                vec!["diff".into(), "--no-color".into(), "-U3".into(), "--".into()];
            a2.extend(files.iter().cloned());
            let s2: Vec<&str> = a2.iter().map(|s| s.as_str()).collect();
            if let Ok(s) = git(repo, &s2) {
                out.push_str(&s);
            }
            // Untracked file contents as synthetic diffs
            for f in files {
                if git(repo, &["ls-files", "--error-unmatch", f]).is_err() {
                    if let Ok(body) = std::fs::read_to_string(repo.join(f)) {
                        out.push_str(&format!("diff --git a/{f} b/{f}\n--- /dev/null\n+++ b/{f}\n"));
                        for (i, line) in body.lines().enumerate() {
                            if i == 0 {
                                out.push_str(&format!("@@ -0,0 +1,{} @@\n", body.lines().count()));
                            }
                            out.push('+');
                            out.push_str(line);
                            out.push('\n');
                        }
                    }
                }
            }
            return Ok(out);
        }
    }
    args.push("--".into());
    args.extend(files.iter().cloned());
    let sargs: Vec<&str> = args.iter().map(|s| s.as_str()).collect();
    git(repo, &sargs)
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
            "--cargo-check" => {
                opts.run_cargo_check = true;
                i += 1;
            }
            "--json" => i += 1,
            _ => i += 1,
        }
    }
    opts
}

pub fn wants_json(args: &[&str]) -> bool {
    args.iter().any(|a| *a == "--json")
}

/// Host-query entry: arg is `cwd` or `cwd|from|to` or empty (process cwd).
/// Returns a single-element vec with the reviewer context (or error:…).
pub fn host_review_report(arg: &str) -> Vec<String> {
    let (cwd, opts) = parse_host_arg(arg);
    match run_review(&cwd, &opts) {
        Ok(report) => {
            let ctx = format_reviewer_context(&cwd, &report, &opts, DEFAULT_MAX_DIFF_CONTEXT_BYTES);
            let json = format_report_json(&report).unwrap_or_default();
            // Line 0: JSON report (machine). Line 1+: context for subagent (joined).
            vec![json, ctx]
        }
        Err(e) => vec![format!("error:{e}")],
    }
}

fn parse_host_arg(arg: &str) -> (std::path::PathBuf, ReviewSelectOpts) {
    let arg = arg.trim();
    let mut opts = ReviewSelectOpts::dirty_default();
    if arg.is_empty() {
        let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
        return (cwd, opts);
    }
    let parts: Vec<&str> = arg.split('|').collect();
    let cwd = if parts[0].is_empty() {
        std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."))
    } else {
        std::path::PathBuf::from(parts[0])
    };
    if parts.len() >= 3 {
        let from = parts[1].trim();
        let to = parts[2].trim();
        if !from.is_empty() {
            opts.from = Some(from.to_string());
        }
        if !to.is_empty() {
            opts.to = Some(to.to_string());
        }
        opts.include_untracked = false;
    }
    (cwd, opts)
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
            "pub fn ok() {}\n// pre-existing TODO: ignore\npub fn old() { y.unwrap(); }\n",
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
    fn rejects_path_escape() {
        assert!(!is_safe_repo_rel_path("../etc/passwd"));
        assert!(!is_safe_repo_rel_path("/etc/passwd"));
        assert!(!is_safe_repo_rel_path("foo/../../bar"));
        assert!(is_safe_repo_rel_path("src/main.rs"));
    }

    #[test]
    fn select_dirty_excludes_noise_includes_source() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn ok() {}\n// pre-existing TODO: ignore\npub fn old() { y.unwrap(); }\n// FIXME new\n",
        )
        .unwrap();
        std::fs::write(dir.path().join("Cargo.lock"), "lock2\n").unwrap();
        std::fs::write(dir.path().join("new_feature.rs"), "fn n() {}\n").unwrap();

        let files = select_changed_files(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(
            files
                .iter()
                .any(|f| f.ends_with("lib.rs") || f == "src/lib.rs"),
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
    fn heuristics_only_flag_added_lines_not_preexisting() {
        let dir = init_repo();
        // Only add a new TODO line; pre-existing unwrap/TODO must not appear.
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn ok() {}\n// pre-existing TODO: ignore\npub fn old() { y.unwrap(); }\n// TODO: only-this\n",
        )
        .unwrap();
        let findings =
            heuristic_findings(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(
            findings
                .iter()
                .any(|f| f.kind == "todo" && f.message.contains("only-this")),
            "expected new TODO: {findings:?}"
        );
        assert!(
            !findings.iter().any(|f| f.kind == "unwrap"),
            "pre-existing unwrap must not be flagged: {findings:?}"
        );
        assert!(
            !findings
                .iter()
                .any(|f| f.kind == "todo" && f.message.contains("pre-existing")),
            "pre-existing TODO must not be flagged: {findings:?}"
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
                slice: None,
            },
            Finding {
                path: "b.rs".into(),
                line: None,
                severity: FindingSeverity::Info,
                kind: "guess".into(),
                message: "speculative".into(),
                slice: None,
            },
            Finding {
                path: "c.rs".into(),
                line: Some(2),
                severity: FindingSeverity::Nit,
                kind: "todo".into(),
                message: "   ".into(),
                slice: None,
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
            slice: Some("  10| x\n".into()),
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
        assert!(!review_tool_allowed("project"));
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
        let ctx = format_reviewer_context(
            dir.path(),
            &report,
            &ReviewSelectOpts::dirty_default(),
            8000,
        );
        assert!(ctx.contains("tool diet") || ctx.contains("DENIED"));
        assert!(ctx.contains("unwrap") || ctx.contains("TODO") || ctx.contains("findings"));
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
        let files =
            select_changed_files(dir.path(), &ReviewSelectOpts::range("HEAD~1", "HEAD")).unwrap();
        assert!(
            files.iter().any(|f| f.contains("extra.rs")),
            "range files: {files:?}"
        );
    }

    #[test]
    fn host_review_report_returns_json_and_context() {
        let dir = init_repo();
        std::fs::write(dir.path().join("src/lib.rs"), "fn z() { a.unwrap(); }\n").unwrap();
        let lines = host_review_report(&dir.path().display().to_string());
        assert!(lines.len() >= 2, "{lines:?}");
        assert!(!lines[0].starts_with("error:"), "{lines:?}");
        let v: serde_json::Value = serde_json::from_str(&lines[0]).unwrap();
        assert!(v.get("files").is_some());
        assert!(v.get("tool_diet").is_some());
        assert!(lines[1].contains("DENIED") || lines[1].contains("denied"));
    }

    #[test]
    fn parse_name_status_nul_handles_rename() {
        let raw = "R100\0old.rs\0new.rs\0M\0src/a.rs\0";
        let paths = parse_name_status_nul(raw);
        assert!(paths.iter().any(|p| p == "new.rs"), "{paths:?}");
        assert!(paths.iter().any(|p| p == "src/a.rs"), "{paths:?}");
        assert!(!paths.iter().any(|p| p == "old.rs"), "{paths:?}");
    }

    #[test]
    fn risk_ranks_security_paths_above_tests() {
        let dir = init_repo();
        std::fs::create_dir_all(dir.path().join("src/auth")).unwrap();
        std::fs::write(dir.path().join("src/auth/login.rs"), "unsafe fn x() {}\n").unwrap();
        std::fs::create_dir_all(dir.path().join("tests")).unwrap();
        std::fs::write(dir.path().join("tests/t.rs"), "fn t() {}\n").unwrap();
        let mut units = partition_units(&[
            "src/auth/login.rs".into(),
            "tests/t.rs".into(),
        ]);
        enrich_units(dir.path(), &mut units);
        assert!(units[0].risk_score >= units[1].risk_score, "{units:?}");
        assert!(
            units[0].paths.iter().any(|p| p.contains("auth")),
            "highest risk should include auth: {units:?}"
        );
        assert!(!units[0].content_hash.is_empty());
    }

    #[test]
    fn slices_attached_to_findings() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "fn a() {}\nfn b() { x.unwrap(); }\nfn c() {}\n",
        )
        .unwrap();
        let report = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        let un = report
            .findings
            .iter()
            .find(|f| f.kind == "unwrap")
            .expect("unwrap finding");
        assert!(un.slice.is_some(), "slice missing: {un:?}");
        assert!(
            un.slice.as_ref().unwrap().contains("unwrap"),
            "slice: {:?}",
            un.slice
        );
    }

    #[test]
    fn cascade_and_fingerprint_present() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn bad() { x.unwrap(); }\n// TODO: fix\n",
        )
        .unwrap();
        let report = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(!report.rubric.plan_fingerprint.is_empty());
        let text = format_report_text(&report);
        assert!(
            text.contains("CASCADE:") || text.contains("residual_risk"),
            "{text}"
        );
        // Second identical review within hysteresis window should skip.
        let report2 = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(
            report2.rubric.hysteresis_skip,
            "expected hysteresis on second fire: {:?}",
            report2.rubric
        );
        assert!(!report2.rubric.needs_llm, "hysteresis implies skip_llm");
    }

    #[test]
    fn scan_line_matrix_secret_shell_panic_unwrap_clean() {
        let secret = scan_added_line("src/a.rs", 1, r#"let secret = "hunter2";"#);
        assert!(
            secret.iter().any(|f| f.kind == "secret" && f.is_blocking()),
            "{secret:?}"
        );
        let shell = scan_added_line(
            "src/a.rs",
            2,
            r#"std::process::Command::new("sh").arg("-c").arg(cmd);"#,
        );
        assert!(
            shell.iter().any(|f| f.kind == "shell_injection"),
            "{shell:?}"
        );
        let pan = scan_added_line("src/a.rs", 3, r#"panic!("nope");"#);
        assert!(pan.iter().any(|f| f.kind == "panic"), "{pan:?}");
        let un = scan_added_line("src/a.rs", 4, "s.parse().unwrap()");
        assert!(un.iter().any(|f| f.kind == "unwrap"), "{un:?}");
        let clean = scan_added_line("src/a.rs", 5, "let x = password_from_env()?;");
        assert!(
            !clean.iter().any(|f| f.kind == "secret"),
            "env password should not flag: {clean:?}"
        );
    }

    #[test]
    fn fixture_matrix_bad_code_auto_blocks() {
        let dir = init_repo();
        std::fs::write(
            dir.path().join("src/lib.rs"),
            r#"
pub fn authenticate(user: &str, password: &str) -> bool {
    let secret = "hunter2";
    if password == secret { true } else { panic!("bad {}", user); }
}
pub fn parse_port(s: &str) -> u16 { s.parse().unwrap() }
pub fn run_cmd(cmd: &str) {
    let _ = std::process::Command::new("sh").arg("-c").arg(cmd).output().unwrap();
}
"#,
        )
        .unwrap();
        let report = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(!report.files.is_empty());
        assert!(
            report.findings.iter().any(|f| f.kind == "secret"),
            "secret: {:?}",
            report.findings
        );
        assert!(
            report
                .findings
                .iter()
                .any(|f| f.kind == "shell_injection" || f.kind == "unwrap"),
            "{:?}",
            report.findings
        );
        let action = decide_gate_action(&report);
        // secret is Error → AutoBlock
        assert_eq!(action, GateAction::AutoBlock, "rubric={:?}", report.rubric);
        let ctx = format_reviewer_context(
            dir.path(),
            &report,
            &ReviewSelectOpts::dirty_default(),
            12_000,
        );
        assert!(ctx.contains("### file:"), "full file context missing: {ctx}");
        assert!(ctx.contains("AUTO_BLOCK") || ctx.contains("gate_action"));
    }

    #[test]
    fn fixture_matrix_clean_and_empty() {
        let dir = init_repo();
        // clean: only whitespace-safe edit
        std::fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn ok() -> i32 {\n    let password = std::env::var(\"PW\").unwrap_or_default();\n    password.len() as i32\n}\n",
        )
        .unwrap();
        // commit so dirty is only if we change - actually this is dirty vs init which had different content
        let report = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(!report.files.is_empty());
        // env-based password should not create secret findings
        assert!(
            !report.findings.iter().any(|f| f.kind == "secret"),
            "{:?}",
            report.findings
        );
        // empty: commit clean
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        };
        run(&["add", "-A"]);
        run(&["commit", "-m", "clean"]);
        let empty = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(empty.files.is_empty(), "{:?}", empty.files);
        assert_eq!(decide_gate_action(&empty), GateAction::PassEmpty);
    }

    #[test]
    fn gate_actions_hysteresis_and_skip_llm() {
        let dir = init_repo();
        std::fs::write(dir.path().join("src/lib.rs"), "pub fn n() { 1 }\n").unwrap();
        let r1 = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        let a1 = decide_gate_action(&r1);
        // low risk nit-free change may be SkipLlm or NeedsLlm depending on residual
        assert!(
            matches!(
                a1,
                GateAction::SkipLlmLowRisk | GateAction::NeedsLlm | GateAction::PassEmpty
            ),
            "{a1:?} {:?}",
            r1.rubric
        );
        let r2 = run_review(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert_eq!(decide_gate_action(&r2), GateAction::SkipHysteresis);
    }

    #[test]
    fn merge_verdicts_requires_both_critical() {
        assert_eq!(merge_llm_verdicts("CRITICAL\n- x", "CRITICAL\n- y"), "CRITICAL");
        assert_eq!(merge_llm_verdicts("CRITICAL\n- x", "SOUND\n- ok"), "SOUND");
        assert_eq!(merge_llm_verdicts("SOUND", "SOUND"), "SOUND");
        assert_eq!(
            merge_llm_verdicts("## **CRITICAL**\nfoo", "CRITICAL"),
            "CRITICAL"
        );
    }

    #[test]
    fn dismissed_findings_filtered() {
        let f = Finding {
            path: "a.rs".into(),
            line: Some(1),
            severity: FindingSeverity::Warning,
            kind: "unwrap".into(),
            message: "x".into(),
            slice: None,
        };
        let key = finding_dismiss_key(&f);
        let mut set = BTreeSet::new();
        set.insert(key);
        let kept = filter_dismissed(vec![f], &set);
        assert!(kept.is_empty());
    }

    #[test]
    fn rename_appears_in_selection() {
        let dir = init_repo();
        let run = |args: &[&str]| {
            assert!(Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .status()
                .unwrap()
                .success());
        };
        run(&["mv", "src/lib.rs", "src/moved.rs"]);
        run(&["add", "-A"]);
        run(&["commit", "-m", "rename"]);
        // dirty empty after commit
        let empty = select_changed_files(dir.path(), &ReviewSelectOpts::dirty_default()).unwrap();
        assert!(empty.is_empty());
        // range should see rename target
        let files =
            select_changed_files(dir.path(), &ReviewSelectOpts::range("HEAD~1", "HEAD")).unwrap();
        assert!(
            files.iter().any(|f| f.contains("moved.rs")),
            "rename target missing: {files:?}"
        );
    }
}
