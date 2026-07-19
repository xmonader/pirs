//! Fault localization: turn a failing test's output into a ranked list of
//! candidate edit sites, and select the scoped test ring around them.
//!
//! Two inputs, two graph-backed steps:
//!  1. **Parse** the traceback/panic into ordered [`Frame`]s (file + line, and a
//!     symbol when the format carries one). This is pure text work — no graph.
//!  2. **Rank** those frames into [`Candidate`] edit sites, using the code graph
//!     to prefer project source over stdlib/vendored code and to confirm that a
//!     frame's symbol is actually defined where the trace says it is.
//!
//! The scoped ring ([`scoped_tests`]) is the union of `graph.affected_tests` over
//! the candidate files — the *affected modules* ring from the plan, kept shallow
//! (direct callers) so cost stays bounded.
//!
//! Localization is advisory: it narrows where the executor looks and which tests
//! run in the scoped ring. It never decides success — the [`gate`](crate::gate)
//! does, over real red→green flips. A miss here costs a wider search, not a wrong
//! answer.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use pirs_graph::Graph;

/// One stack frame extracted from failure output, innermost-last (the order the
/// traceback presents them, which for Python/pytest means the error site is the
/// final frame).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Frame {
    pub file: PathBuf,
    pub line: usize,
    /// The enclosing function/symbol, when the trace format names it.
    pub symbol: Option<String>,
}

/// A ranked candidate edit site. Higher `score` = more likely the fix belongs
/// here. Deduped to one entry per file (the best-scoring frame in that file).
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub file: PathBuf,
    pub symbol: Option<String>,
    pub score: f64,
}

/// Source extensions we recognize in a `path.ext:line` trace fragment.
const CODE_EXTS: &[&str] = &["py", "rs", "go", "js", "ts", "rb", "java"];

/// Path fragments that mark a frame as *not* project code — stdlib, vendored
/// deps, or virtualenvs. Frames in these are almost never the fix site.
const VENDOR_MARKERS: &[&str] = &[
    "site-packages",
    "dist-packages",
    "/lib/python",
    "/usr/lib/",
    "node_modules",
    "/.cargo/",
    "/rustc/",
    "/go/pkg/mod/",
];

/// Parse a traceback / panic / test log into ordered frames. Handles the common
/// shapes across ecosystems; unknown lines are ignored rather than guessed at.
pub fn parse_traceback(output: &str) -> Vec<Frame> {
    let mut frames = Vec::new();
    for raw in output.lines() {
        let line = raw.trim_end();
        if let Some(f) = parse_python_file_line(line).or_else(|| parse_generic_file_line(line)) {
            frames.push(f);
        }
    }
    frames
}

/// Python's `  File "/path/foo.py", line 42, in bar` frame.
fn parse_python_file_line(line: &str) -> Option<Frame> {
    let t = line.trim_start();
    let rest = t.strip_prefix("File \"")?;
    let (path, after) = rest.split_once('"')?;
    let after = after.strip_prefix(", line ")?;
    let (num, tail) = split_leading_number(after)?;
    let symbol = tail
        .strip_prefix(", in ")
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
    Some(Frame { file: PathBuf::from(path), line: num, symbol })
}

/// Generic `<path>.<ext>:<line>` fragment (pytest `path.py:12: in f`, Rust
/// `panicked at src/x.rs:3:5`, Go `\tfile.go:9 +0x1a`, backtrace `at ./x.rs:4`).
/// Scans for the last recognized `ext:` marker on the line so a leading
/// `at `/tab/`panicked at ` prefix doesn't matter.
fn parse_generic_file_line(line: &str) -> Option<Frame> {
    // Find the rightmost ".<ext>:" whose ext is a code extension.
    let mut best: Option<(usize, usize)> = None; // (ext_dot_idx, colon_idx)
    for ext in CODE_EXTS {
        let needle = format!(".{ext}:");
        if let Some(pos) = line.rfind(&needle) {
            let colon = pos + needle.len() - 1;
            if best.map(|(_, c)| colon > c).unwrap_or(true) {
                best = Some((pos, colon));
            }
        }
    }
    let (_dot, colon) = best?;
    // Walk left from the ext to the start of the path token (stop at whitespace,
    // quote, paren, or the pytest/Go leading markers).
    let path_start = line[..colon]
        .rfind(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '(' | '\t'))
        .map(|i| i + 1)
        .unwrap_or(0);
    let path = &line[path_start..colon];
    if path.is_empty() {
        return None;
    }
    let after = &line[colon + 1..];
    let (num, tail) = split_leading_number(after)?;
    // pytest form: "path.py:12: in func"
    let symbol = tail
        .trim_start_matches(|c: char| c == ':' || c.is_whitespace())
        .strip_prefix("in ")
        .map(|s| s.split_whitespace().next().unwrap_or("").to_string())
        .filter(|s| !s.is_empty());
    Some(Frame { file: strip_path_prefix(path), line: num, symbol })
}

/// Drop a leading `./` so paths match how the graph stores them (relative).
fn strip_path_prefix(p: &str) -> PathBuf {
    PathBuf::from(p.strip_prefix("./").unwrap_or(p))
}

/// Split a leading run of ASCII digits off the front, returning (number, rest).
fn split_leading_number(s: &str) -> Option<(usize, &str)> {
    let end = s.find(|c: char| !c.is_ascii_digit()).unwrap_or(s.len());
    if end == 0 {
        return None;
    }
    let num: usize = s[..end].parse().ok()?;
    Some((num, &s[end..]))
}

fn is_vendored(path: &Path) -> bool {
    let s = path.to_string_lossy();
    VENDOR_MARKERS.iter().any(|m| s.contains(m))
}

fn is_test_path(path: &Path) -> bool {
    let name = path.file_name().and_then(|f| f.to_str()).unwrap_or("");
    name.starts_with("test_")
        || name.ends_with("_test.py")
        || name.ends_with("_test.go")
        || name.ends_with("_test.rs")
        || path.components().any(|c| c.as_os_str() == "tests")
}

/// Rank frames into candidate edit sites. `root` is the repo root; frames whose
/// files resolve under it (and aren't vendored) are project code and score
/// higher. Returns one [`Candidate`] per file, best-scoring first.
///
/// Scoring, all multiplicative so a disqualifier can't be out-weighted by depth:
///  - **depth**: innermost frame scores highest (fix is usually near the throw).
///  - **project**: project source ×1, vendored/stdlib ×0.1 (rarely the fix).
///  - **role**: test files ×0.4 (they reproduce; the fix is usually elsewhere).
///  - **graph**: symbol actually defined in this file ×1.5 (confirmed site).
pub fn rank_candidates(graph: &Graph, frames: &[Frame], root: &Path) -> Vec<Candidate> {
    let n = frames.len();
    let mut best: Vec<Candidate> = Vec::new();
    for (i, f) in frames.iter().enumerate() {
        let abs = if f.file.is_absolute() { f.file.clone() } else { root.join(&f.file) };
        // Depth: later frame → higher base in (0, 1].
        let depth = (i + 1) as f64 / n as f64;
        let mut score = depth;
        if is_vendored(&abs) || !abs.starts_with(root) {
            score *= 0.1;
        }
        if is_test_path(&f.file) {
            score *= 0.4;
        }
        if let Some(sym) = &f.symbol {
            if graph.find_definition(sym, &abs).is_some()
                || graph.find_definition(sym, &f.file).is_some()
            {
                score *= 1.5;
            }
        }
        // Keep the best score per file.
        match best.iter_mut().find(|c| c.file == f.file) {
            Some(c) if score > c.score => {
                c.score = score;
                c.symbol = f.symbol.clone();
            }
            Some(_) => {}
            None => best.push(Candidate { file: f.file.clone(), symbol: f.symbol.clone(), score }),
        }
    }
    best.sort_by(|a, b| b.score.partial_cmp(&a.score).unwrap_or(std::cmp::Ordering::Equal));
    best
}

/// The scoped test ring: the union of tests affected by an edit to any candidate
/// file, per the code graph's (shallow, direct-caller) `affected_tests`. Paths
/// are resolved against `root` before the graph lookup. Deduped, sorted.
pub fn scoped_tests(graph: &Graph, files: &[PathBuf], root: &Path) -> Vec<String> {
    let mut out: BTreeSet<String> = BTreeSet::new();
    for f in files {
        let abs = if f.is_absolute() { f.clone() } else { root.join(f) };
        for t in graph.affected_tests(&abs) {
            out.insert(t);
        }
        // Also try the raw (relative) path in case the graph indexed it that way.
        if abs != *f {
            for t in graph.affected_tests(f) {
                out.insert(t);
            }
        }
    }
    out.into_iter().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_python_traceback_frames_in_order() {
        let tb = r#"Traceback (most recent call last):
  File "/repo/app/main.py", line 10, in run
    do_thing()
  File "/repo/app/core.py", line 42, in do_thing
    raise ValueError("boom")
ValueError: boom
"#;
        let frames = parse_traceback(tb);
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].file, PathBuf::from("/repo/app/main.py"));
        assert_eq!(frames[0].line, 10);
        assert_eq!(frames[0].symbol.as_deref(), Some("run"));
        // Innermost frame is last.
        assert_eq!(frames[1].symbol.as_deref(), Some("do_thing"));
        assert_eq!(frames[1].line, 42);
    }

    #[test]
    fn parses_pytest_short_style() {
        let out = "app/core.py:42: in do_thing\n    raise ValueError\nE   ValueError: boom";
        let frames = parse_traceback(out);
        assert_eq!(frames[0].file, PathBuf::from("app/core.py"));
        assert_eq!(frames[0].line, 42);
        assert_eq!(frames[0].symbol.as_deref(), Some("do_thing"));
    }

    #[test]
    fn parses_rust_panic_and_backtrace() {
        let out = "thread 'main' panicked at src/lib.rs:12:9:\n   3: mycrate::foo\n             at ./src/foo.rs:44";
        let frames = parse_traceback(out);
        assert_eq!(frames[0].file, PathBuf::from("src/lib.rs"));
        assert_eq!(frames[0].line, 12);
        assert_eq!(frames[1].file, PathBuf::from("src/foo.rs"));
        assert_eq!(frames[1].line, 44);
    }

    #[test]
    fn parses_go_frame() {
        let out = "panic: boom\n\t/repo/pkg/thing.go:88 +0x1a5";
        let frames = parse_traceback(out);
        assert_eq!(frames[0].file, PathBuf::from("/repo/pkg/thing.go"));
        assert_eq!(frames[0].line, 88);
    }

    #[test]
    fn non_frame_lines_are_ignored() {
        let out = "just some prose\nE   AssertionError: 1 != 2\n----\n";
        assert!(parse_traceback(out).is_empty());
    }

    #[test]
    fn vendored_frames_score_below_project_frames() {
        let root = Path::new("/repo");
        let g = empty_graph();
        let frames = vec![
            Frame { file: PathBuf::from("/usr/lib/python3.11/json/decoder.py"), line: 5, symbol: None },
            Frame { file: PathBuf::from("/repo/app/core.py"), line: 42, symbol: None },
        ];
        let ranked = rank_candidates(&g, &frames, root);
        // Project frame wins despite being innermost-vendored competitor.
        assert_eq!(ranked[0].file, PathBuf::from("/repo/app/core.py"));
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn test_files_rank_below_source_files() {
        let root = Path::new("/repo");
        let g = empty_graph();
        let frames = vec![
            Frame { file: PathBuf::from("/repo/app/core.py"), line: 42, symbol: None },
            Frame { file: PathBuf::from("/repo/tests/test_core.py"), line: 9, symbol: None },
        ];
        let ranked = rank_candidates(&g, &frames, root);
        assert_eq!(ranked[0].file, PathBuf::from("/repo/app/core.py"));
    }

    #[test]
    fn scoped_tests_uses_graph_affected_tests() {
        // Build a real graph from a tiny python repo: a source function and a
        // test that calls it. Editing the source should surface the test.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("app")).unwrap();
        std::fs::create_dir_all(root.join("tests")).unwrap();
        std::fs::write(root.join("app/core.py"), "def do_thing():\n    return 1\n").unwrap();
        std::fs::write(
            root.join("tests/test_core.py"),
            "from app.core import do_thing\n\ndef test_do_thing():\n    assert do_thing() == 1\n",
        )
        .unwrap();
        let g = Graph::build(root);
        let tests = scoped_tests(&g, &[PathBuf::from("app/core.py")], root);
        assert!(
            tests.iter().any(|t| t == "test_do_thing"),
            "expected test_do_thing in scoped ring, got {tests:?}"
        );
    }

    // A Graph with no symbols; fields are pub so we can build one directly for
    // the scoring tests that don't need a real parse.
    fn empty_graph() -> Graph {
        Graph::build(Path::new("/nonexistent-empty-root-for-tests"))
    }
}
