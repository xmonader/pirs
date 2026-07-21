use std::path::PathBuf;

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};
use unicode_normalization::UnicodeNormalization;

use crate::paths;

#[derive(Deserialize, JsonSchema)]
struct EditOp {
    /// Exact text to find in the file
    #[serde(rename = "oldText")]
    old_text: String,
    /// Replacement text
    #[serde(rename = "newText")]
    new_text: String,
}

#[derive(Deserialize, JsonSchema)]
struct EditArgs {
    /// Path to the file to edit
    path: String,
    /// List of replacements; each oldText must match exactly one location
    edits: Vec<EditOp>,
}

pub struct EditTool {
    cwd: PathBuf,
}

impl EditTool {
    pub fn new(cwd: PathBuf) -> Self {
        EditTool { cwd }
    }
}

#[async_trait]
impl AgentTool for EditTool {
    fn name(&self) -> &str {
        "edit"
    }

    fn description(&self) -> &str {
        "Make exact string replacements in a file. Each oldText must match exactly one location; if an exact match isn't found, whitespace/formatting differences are tolerated (quote/dash style, indentation, reflowed spacing) before failing. Edits are validated against the original file and must not overlap."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(EditArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("edit: replace exact text blocks in a file (oldText/newText pairs)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: EditArgs = serde_json::from_value(ctx.args)?;
        if args.edits.is_empty() {
            bail!("edits must contain at least one replacement");
        }
        let path = paths::resolve_contained(&self.cwd, &args.path)?;
        let _mutation_guard = crate::filelock::lock(&path).await;
        let raw =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let original = String::from_utf8_lossy(&raw).into_owned();

        let Norm {
            body,
            without_bom,
            bom,
            crlf,
            map,
        } = normalize_file(&original);
        let mut spans: Vec<(usize, usize, String)> = Vec::new();
        for op in &args.edits {
            if op.old_text.is_empty() {
                bail!("oldText must not be empty");
            }
            let span =
                locate(&body, &op.old_text).ok_or_else(|| not_found_error(&body, &op.old_text))?;
            if body[span.0..span.1] == op.new_text {
                bail!("newText is identical to the matched text; no change");
            }
            spans.push((span.0, span.1, op.new_text.clone()));
        }

        let mut order: Vec<usize> = (0..spans.len()).collect();
        order.sort_by_key(|&i| spans[i].0);
        for w in order.windows(2) {
            if spans[w[1]].0 < spans[w[0]].1 {
                bail!("edits overlap and cannot be applied together");
            }
        }

        // Apply from the highest byte offset down so each replacement leaves
        // earlier offsets valid. Must iterate in sorted position order (`order`),
        // NOT input order reversed — edits supplied later-position-first would
        // otherwise apply at stale offsets and corrupt the file (or panic).
        // Apply edits to the ORIGINAL bytes (via the offset map) rather than to
        // the LF-normalized body, so untouched lines keep their exact original
        // endings. New content inherits the line-ending style of the region it
        // replaces (or the file's dominant style when the region has none), so a
        // mixed-EOL file is not silently rewritten to a single style.
        let mut edited_orig = without_bom.clone();
        for &i in order.iter().rev() {
            let (bstart, bend, new) = &spans[i];
            let ostart = map[*bstart];
            let oend = map[*bend];
            let region = &without_bom[ostart..oend];
            let use_crlf = region.contains("\r\n")
                || (crlf && !region.contains('\n') && !region.contains('\r'));
            let mut replacement = new.clone();
            if oend > ostart && without_bom[..oend].ends_with('\n') && !replacement.ends_with('\n')
            {
                replacement.push('\n');
            }
            if use_crlf {
                replacement = replacement.replace('\n', "\r\n");
            }
            edited_orig.replace_range(ostart..oend, &replacement);
        }

        let mut out = String::new();
        if bom {
            out.push('\u{feff}');
        }
        out.push_str(&edited_orig);
        std::fs::write(&path, out.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;

        // Report the diff in LF terms so line-ending bytes never show up as
        // spurious changes in the patch.
        let edited_lf = edited_orig.replace("\r\n", "\n");
        let first_changed = first_changed_line(&body, &edited_lf);
        let patch = unified_patch(&body, &edited_lf, &args.path);
        // Model sees short ack; UI/audit get the unified diff (A1 show-diffs).
        let summary = format!(
            "Successfully replaced {} block(s) in {}",
            spans.len(),
            path.display()
        );
        let ui = if patch.is_empty() {
            summary.clone()
        } else {
            format!("{summary}\n\n{patch}")
        };
        Ok(ToolOutput::text_with_ui(summary, Some(ui)).with_details(json!({
            "path": path.display().to_string(),
            "patch": patch,
            "firstChangedLine": first_changed,
        })))
    }
}

/// Result of normalizing a file for editing. `body` is LF-normalized so that
/// `oldText`/`newText` (which use `\n`) match regardless of the file's line
/// endings, while `without_bom` keeps the file's exact original bytes so edits
/// can be written back without disturbing the line endings of untouched lines.
/// `map[i]` is the byte offset in `without_bom` that body byte `i` came from
/// (length `body.len() + 1`, with a trailing sentinel = `without_bom.len()`).
struct Norm {
    body: String,
    without_bom: String,
    bom: bool,
    crlf: bool,
    map: Vec<usize>,
}

fn normalize_file(content: &str) -> Norm {
    let bom = content.starts_with('\u{feff}');
    let without_bom = if bom { &content[3..] } else { content };
    let crlf = without_bom.contains("\r\n");
    let body = if crlf {
        without_bom.replace("\r\n", "\n")
    } else {
        without_bom.to_string()
    };

    // Build the body→original offset map. Only `\r\n` is collapsed (to a single
    // `\n`), so every body byte corresponds to one original byte except the
    // `\n` of a former `\r\n`, which maps to the `\r`; the next entry then
    // naturally points past the pair. Lone `\r` and `\n` pass through 1:1.
    let ob = without_bom.as_bytes();
    let mut map = Vec::with_capacity(body.len() + 1);
    let mut oi = 0usize;
    while oi < ob.len() {
        map.push(oi);
        if ob[oi] == b'\r' && ob.get(oi + 1) == Some(&b'\n') {
            oi += 2;
        } else {
            oi += 1;
        }
    }
    map.push(ob.len());

    Norm {
        body,
        without_bom: without_bom.to_string(),
        bom,
        crlf,
        map,
    }
}

fn locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    let matches: Vec<usize> = content.match_indices(old_text).map(|(i, _)| i).collect();
    match matches.len() {
        1 => return Some((matches[0], matches[0] + old_text.len())),
        n if n > 1 => return None,
        _ => {}
    }
    // Escalating fuzzy tiers, aider-style: each is strictly more lenient than
    // the last, tried only after the previous one fails to resolve to a
    // single unique hit (0 hits or >1 ambiguous hits both fall through the
    // same way, mirroring the exact tier above). Every tier still requires
    // uniqueness — a more lenient match is never allowed to silently pick
    // among several candidates.
    fuzzy_locate(content, old_text)
        .or_else(|| reflow_locate(content, old_text))
        // Multi-line SequenceMatcher-style window (aider's last resort).
        .or_else(|| similarity_locate(content, old_text))
}

/// Apply a single old→new replacement to file contents (LF-normalized body).
/// Shared by `edit` and `edit_block`. Returns the new full file text.
pub fn apply_one_replacement(original: &str, old_text: &str, new_text: &str) -> anyhow::Result<String> {
    if old_text.is_empty() {
        bail!("search/oldText must not be empty");
    }
    let Norm {
        body,
        without_bom,
        bom,
        crlf,
        map,
    } = normalize_file(original);
    let span = locate(&body, old_text).ok_or_else(|| not_found_error(&body, old_text))?;
    if &body[span.0..span.1] == new_text {
        bail!("replacement is identical to the matched text; no change");
    }
    let ostart = map[span.0];
    let oend = map[span.1];
    let region = &without_bom[ostart..oend];
    let use_crlf =
        region.contains("\r\n") || (crlf && !region.contains('\n') && !region.contains('\r'));
    let mut replacement = new_text.to_string();
    if oend > ostart && without_bom[..oend].ends_with('\n') && !replacement.ends_with('\n') {
        replacement.push('\n');
    }
    if use_crlf {
        replacement = replacement.replace('\n', "\r\n");
    }
    let mut edited_orig = without_bom;
    edited_orig.replace_range(ostart..oend, &replacement);
    let mut out = String::new();
    if bom {
        out.push('\u{feff}');
    }
    out.push_str(&edited_orig);
    Ok(out)
}

fn not_found_error(content: &str, old_text: &str) -> anyhow::Error {
    let exact = content.match_indices(old_text).count();
    if exact > 1 {
        return anyhow::anyhow!(
            "oldText occurs {exact} times in the file; it must be unique. Add more surrounding context (include surrounding lines so the match is unique)."
        );
    }
    let mut msg = String::from(
        "oldText not found in the file (exact and fuzzy match both failed). \
         Re-read the file, copy the exact current text, and retry with a larger unique block.",
    );
    // Show a short preview of what the model tried to match.
    let preview: String = old_text.chars().take(120).collect();
    let ellipsis = if old_text.chars().count() > 120 {
        "…"
    } else {
        ""
    };
    msg.push_str(&format!("\n\nYour oldText began with:\n  | {preview}{ellipsis}"));

    // Suggest nearest lines by fuzzy-normalized token overlap (cheap, no deps).
    let candidates = similar_line_candidates(content, old_text, 3);
    if !candidates.is_empty() {
        msg.push_str("\n\nClosest lines in the file (did you mean one of these?):");
        for (line_no, line) in candidates {
            let shown: String = line.chars().take(100).collect();
            let e = if line.chars().count() > 100 { "…" } else { "" };
            msg.push_str(&format!("\n  L{line_no}: {shown}{e}"));
        }
        msg.push_str(
            "\n\nTip: include 2–3 surrounding lines in oldText so the match is unique, \
             or replace the entire function body.",
        );
    }
    anyhow::anyhow!(msg)
}

/// Rank file lines by shared whitespace-normalized tokens with `old_text`.
fn similar_line_candidates(content: &str, old_text: &str, limit: usize) -> Vec<(usize, String)> {
    let needle_tokens = token_set(&reflow_normalize(old_text));
    if needle_tokens.is_empty() {
        return Vec::new();
    }
    let mut scored: Vec<(usize, usize, String)> = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let tokens = token_set(&reflow_normalize(line));
        let overlap = needle_tokens.intersection(&tokens).count();
        if overlap == 0 {
            continue;
        }
        scored.push((overlap, i + 1, line.to_string()));
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(limit)
        .map(|(_, n, l)| (n, l))
        .collect()
}

fn token_set(s: &str) -> std::collections::BTreeSet<String> {
    s.split_whitespace()
        .filter(|t| t.len() >= 2)
        .map(|t| t.to_string())
        .collect()
}

pub fn fuzzy_normalize(line: &str) -> String {
    let nfkc: String = line.nfkc().collect();
    let mut out = String::with_capacity(nfkc.len());
    for c in nfkc.chars() {
        match c {
            '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}' => out.push('\''),
            '\u{201c}' | '\u{201d}' | '\u{201e}' | '\u{201f}' => out.push('"'),
            '\u{2013}' | '\u{2014}' | '\u{2015}' => out.push('-'),
            '\u{00a0}' | '\u{2000}'..='\u{200a}' | '\u{202f}' | '\u{205f}' | '\u{3000}' => {
                out.push(' ')
            }
            _ => out.push(c),
        }
    }
    out.trim_end().to_string()
}

/// More lenient still than `fuzzy_normalize`: collapses each line down to its
/// whitespace-separated tokens joined by a single space, which also strips
/// leading indentation (not just trailing whitespace) and folds internal
/// runs of spaces/tabs to one. Lets an edit land when only formatting or
/// indentation differs from what the model wrote — e.g. tabs vs spaces, or
/// a reflowed/re-indented block — at the cost of no longer distinguishing
/// lines that differ only in whitespace.
fn reflow_normalize(line: &str) -> String {
    fuzzy_normalize(line)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn locate_lines(
    content: &str,
    old_text: &str,
    normalize: impl Fn(&str) -> String,
) -> Option<(usize, usize)> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<String> = old_text.lines().map(&normalize).collect();
    if old_lines.is_empty() || old_lines.len() > content_lines.len() {
        return None;
    }
    let norm_content: Vec<String> = content_lines.iter().map(|l| normalize(l)).collect();

    let mut hits: Vec<usize> = Vec::new();
    for i in 0..=norm_content.len() - old_lines.len() {
        if norm_content[i..i + old_lines.len()] == old_lines[..] {
            hits.push(i);
        }
    }
    if hits.len() != 1 {
        return None;
    }
    let start_line = hits[0];
    let end_line = start_line + old_lines.len();

    let starts = line_starts(content);
    let start = starts[start_line];
    let end = if end_line < starts.len() {
        starts[end_line]
    } else {
        content.len()
    };
    Some((start, end))
}

fn fuzzy_locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    locate_lines(content, old_text, fuzzy_normalize)
}

fn reflow_locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    locate_lines(content, old_text, reflow_normalize)
}

/// Aider-style multi-line fuzzy match: slide a window of the same line count
/// and accept the unique best window with SequenceMatcher ratio ≥ 0.8.
fn similarity_locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    const THRESH: f64 = 0.8;
    let old_lines: Vec<&str> = old_text.lines().collect();
    if old_lines.is_empty() || old_lines.len() > 80 {
        return None;
    }
    let content_lines: Vec<&str> = content.lines().collect();
    if content_lines.len() < old_lines.len() {
        return None;
    }
    let old_joined = old_lines.join("\n");
    let mut best: Option<(f64, usize)> = None;
    let mut second = 0.0f64;
    let window = old_lines.len();
    for i in 0..=content_lines.len() - window {
        let chunk = content_lines[i..i + window].join("\n");
        // Char-level ratio (aider SequenceMatcher style). Line-level is too
        // harsh when only one token in a multi-line block differs.
        let ratio = f64::from(similar::TextDiff::from_chars(&old_joined, &chunk).ratio());
        if ratio < THRESH {
            continue;
        }
        match best {
            Some((r, _)) if ratio > r + 1e-9 => {
                second = r;
                best = Some((ratio, i));
            }
            Some((r, _)) if (ratio - r).abs() < 1e-9 => {
                // Ambiguous: two windows with same score.
                second = ratio;
            }
            Some((r, _)) => {
                if ratio > second {
                    second = ratio;
                }
                let _ = r;
            }
            None => best = Some((ratio, i)),
        }
    }
    let (best_ratio, start_line) = best?;
    // Require uniqueness: second-best must be clearly worse.
    if second + 0.02 >= best_ratio && second > 0.0 {
        return None;
    }
    // Map line window to byte offsets.
    let starts = line_starts(content);
    let start = *starts.get(start_line)?;
    let end_line = start_line + window;
    let end = if end_line < starts.len() {
        starts[end_line]
    } else {
        content.len()
    };
    // Prefer excluding a trailing final newline mismatch by trimming end to
    // the last line of the window when the file continues.
    Some((start, end.min(content.len())))
}

fn line_starts(content: &str) -> Vec<usize> {
    let mut starts = vec![0];
    for (i, b) in content.bytes().enumerate() {
        if b == b'\n' {
            starts.push(i + 1);
        }
    }
    starts
}

fn first_changed_line(old: &str, new: &str) -> usize {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();
    let mut i = 0;
    while i < old_lines.len() && i < new_lines.len() && old_lines[i] == new_lines[i] {
        i += 1;
    }
    i + 1
}

fn unified_patch(old: &str, new: &str, path: &str) -> String {
    let diff = similar::TextDiff::from_lines(old, new);
    diff.unified_diff()
        .context_radius(4)
        .header(&format!("a/{path}"), &format!("b/{path}"))
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_unique() {
        let content = "fn main() {}\nfn other() {}\n";
        assert_eq!(locate(content, "fn other"), Some((13, 21)));
    }

    #[test]
    fn exact_multiple_rejected() {
        let content = "x = 1;\nx = 1;\n";
        assert!(locate(content, "x = 1;").is_none());
    }

    #[test]
    fn fuzzy_matches_smart_quotes_and_trailing_ws() {
        let content = "let s = \u{201c}hi\u{201d};   \nnext();\n";
        let old = "let s = \"hi\";\nnext();\n";
        let span = fuzzy_locate(content, old).unwrap();
        assert_eq!(&content[span.0..span.1], content);
    }

    #[test]
    fn fuzzy_multiple_rejected() {
        let content = "foo  \nfoo\n";
        assert!(fuzzy_locate(content, "foo").is_none());
    }

    #[test]
    fn similarity_locate_matches_near_miss_block() {
        let content = "fn compute_total(x: i32) -> i32 {\n    x + 1\n}\n";
        // Model almost-right block (wrong constant) — high SequenceMatcher ratio.
        let old = "fn compute_total(x: i32) -> i32 {\n    x + 2\n}";
        let span = similarity_locate(content, old).expect("expected similarity match");
        let matched = &content[span.0..span.1];
        assert!(
            matched.contains("compute_total") && matched.contains("x + 1"),
            "matched={matched:?}"
        );
    }

    #[test]
    fn not_found_error_lists_similar_lines() {
        let content = "fn compute_total(a: i32) -> i32 {\n    a + 1\n}\n";
        let err = not_found_error(content, "fn compute_ttl(a: i32) -> i32 {");
        let msg = format!("{err:#}");
        assert!(
            msg.contains("Closest lines") || msg.contains("compute_total"),
            "expected candidate hint, got: {msg}"
        );
        assert!(msg.contains("oldText not found"));
    }

    #[test]
    fn reflow_locate_matches_despite_reindentation() {
        // fuzzy_locate only trims trailing whitespace, so a leading-indent
        // mismatch still fails there; reflow_locate is the escalation that
        // catches it.
        let content = "fn f() {\n\tif x {\n\t\tdo_thing();\n\t}\n}\n";
        let old = "if x {\n    do_thing();\n}\n";
        assert!(
            fuzzy_locate(content, old).is_none(),
            "fuzzy tier shouldn't match differing indentation"
        );
        let span = reflow_locate(content, old).unwrap();
        assert_eq!(&content[span.0..span.1], "\tif x {\n\t\tdo_thing();\n\t}\n");
    }

    #[test]
    fn reflow_locate_collapses_internal_whitespace_runs() {
        let content = "let   x   =   1;\nnext();\n";
        let old = "let x = 1;\nnext();\n";
        assert!(fuzzy_locate(content, old).is_none());
        let span = reflow_locate(content, old).unwrap();
        assert_eq!(&content[span.0..span.1], content);
    }

    #[test]
    fn reflow_multiple_rejected() {
        let content = "  foo\nfoo  \n";
        // Both lines reflow-normalize to "foo" — still ambiguous, must not
        // silently pick one.
        assert!(reflow_locate(content, "foo").is_none());
    }

    #[test]
    fn locate_escalates_through_all_three_tiers() {
        let content = "fn f() {\n\tif x {\n\t\tdo_thing();\n\t}\n}\n";
        let old = "if x {\n    do_thing();\n}\n";
        // Not found verbatim, not found by fuzzy (trailing-ws-only) — only
        // the reflow tier resolves it, and `locate` must reach that far.
        let span = locate(content, old).unwrap();
        assert_eq!(&content[span.0..span.1], "\tif x {\n\t\tdo_thing();\n\t}\n");
    }

    #[test]
    fn normalize_file_crlf() {
        let n = normalize_file("a\r\nb\r\n");
        assert_eq!(n.body, "a\nb\n");
        assert!(!n.bom);
        assert!(n.crlf);
        // Map: each body byte points back to its original offset. "a\r\nb\r\n"
        // -> body "a\nb\n"; body '\n' (idx 1) maps to the '\r' at orig idx 1.
        assert_eq!(n.map, vec![0, 1, 3, 4, 6]);
    }

    async fn edit_file(dir: &std::path::Path, initial: &[u8], old: &str, new: &str) -> Vec<u8> {
        let path = dir.join("f.txt");
        std::fs::write(&path, initial).unwrap();
        let tool = EditTool::new(dir.to_path_buf());
        tool.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({
                "path": "f.txt",
                "edits": [{"oldText": old, "newText": new}]
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
        std::fs::read(&path).unwrap()
    }

    #[tokio::test]
    async fn crlf_file_keeps_crlf() {
        let dir = tempfile::tempdir().unwrap();
        let out = edit_file(dir.path(), b"a\r\nb\r\nc\r\n", "b", "X").await;
        assert_eq!(out, b"a\r\nX\r\nc\r\n");
    }

    #[tokio::test]
    async fn lf_file_keeps_lf() {
        let dir = tempfile::tempdir().unwrap();
        let out = edit_file(dir.path(), b"a\nb\nc\n", "b", "X").await;
        assert_eq!(out, b"a\nX\nc\n");
    }

    #[tokio::test]
    async fn mixed_eol_untouched_lines_keep_their_endings() {
        // Lines 1 and 3 are CRLF, line 2 is LF. Editing line 3 must NOT rewrite
        // lines 1-2's endings — only the touched region changes.
        let dir = tempfile::tempdir().unwrap();
        let out = edit_file(dir.path(), b"a\r\nb\nc\r\n", "c", "X").await;
        assert_eq!(out, b"a\r\nb\nX\r\n");
    }

    #[tokio::test]
    async fn concurrent_edits_all_land() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "a1\nb2\nc3\nd4\n").unwrap();
        let tool = std::sync::Arc::new(EditTool::new(dir.path().to_path_buf()));

        let mut handles = Vec::new();
        for (old, new) in [("a1", "A"), ("b2", "B"), ("c3", "C"), ("d4", "D")] {
            let tool = std::sync::Arc::clone(&tool);
            handles.push(tokio::spawn(async move {
                tool.execute(pirs_agent::ToolExecContext {
                    tool_call_id: "t".into(),
                    args: serde_json::json!({
                        "path": "f.txt",
                        "edits": [{"oldText": old, "newText": new}]
                    }),
                    cancel: tokio_util::sync::CancellationToken::new(),
                    on_update: None,
                })
                .await
            }));
        }
        for h in handles {
            h.await.unwrap().unwrap();
        }
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "A\nB\nC\nD\n");
    }

    #[tokio::test]
    async fn multi_edit_applies_regardless_of_input_order() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.txt");
        std::fs::write(&path, "first\nsecond\nthird\n").unwrap();
        let tool = EditTool::new(dir.path().to_path_buf());
        // Edits supplied later-position-first: with the old bug this applied at
        // stale offsets (panic or wrong bytes). Both must land correctly.
        tool.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({
                "path": "f.txt",
                "edits": [
                    {"oldText": "third", "newText": "THIRD"},
                    {"oldText": "first", "newText": "FIRST"}
                ]
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(&path).unwrap(),
            "FIRST\nsecond\nTHIRD\n"
        );
    }

    #[tokio::test]
    async fn execute_succeeds_via_reflow_tier_when_indentation_differs() {
        // End-to-end: the model's oldText uses spaces, the file uses tabs —
        // this must still succeed by escalating to reflow_locate rather
        // than erroring out on the first exact-match miss.
        let dir = tempfile::tempdir().unwrap();
        let out = edit_file(
            dir.path(),
            b"fn f() {\n\tif x {\n\t\tdo_thing();\n\t}\n}\n",
            "if x {\n    do_thing();\n}\n",
            "if x {\n    do_other_thing();\n}\n",
        )
        .await;
        assert_eq!(
            out,
            b"fn f() {\nif x {\n    do_other_thing();\n}\n}\n".to_vec()
        );
    }
}
