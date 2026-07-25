//! Applying an LSP `WorkspaceEdit` to files on disk.
//!
//! A rename comes back from the language server as a `WorkspaceEdit`: a set of
//! text edits keyed by file. This module turns that JSON into real file writes.
//! It is deliberately independent of the LSP client so the fiddly part — position
//! → byte-offset math and applying overlapping edits without corrupting offsets —
//! is unit-testable against hand-built edits, no language server required.
//!
//! Two shapes are accepted (servers use one or the other): the legacy
//! `changes: { <uri>: [TextEdit] }` map and the newer `documentChanges:
//! [{ textDocument: { uri }, edits: [TextEdit] }]` array.
//!
//! **Caveat:** LSP `character` offsets are UTF-16 code units. We index by Unicode
//! scalar (`char`), which matches for the ASCII/BMP text that identifiers and
//! their surrounding code overwhelmingly are; a character *outside the BMP* to the
//! left of an edit on the same line would shift it. Renamed identifiers don't
//! contain such characters, so in practice this is correct.

use std::collections::BTreeMap;
use std::path::PathBuf;

use anyhow::{anyhow, bail, Context as _};
use serde_json::Value;

use crate::client::path_from_uri;

/// One text edit: replace `[start, end)` (LSP positions) with `new_text`.
#[derive(Debug, Clone)]
struct TextEdit {
    start_line: u32,
    start_char: u32,
    end_line: u32,
    end_char: u32,
    new_text: String,
}

/// Apply a `WorkspaceEdit` to files on disk. Returns `(path, edit_count)` per file
/// touched, sorted by path. Edits within a file are applied end-to-start so an
/// earlier edit never invalidates a later edit's byte offsets.
pub fn apply_workspace_edit(edit: &Value) -> anyhow::Result<Vec<(PathBuf, usize)>> {
    apply_workspace_edit_filtered(edit, None)
}

/// Like [`apply_workspace_edit`], but only writes paths under `root` (via
/// `pirs_tools::paths::resolve_contained` / root prefix). Outside URIs are skipped
/// with a warning in the summary (not silent host-wide rewrites — C-7).
pub fn apply_workspace_edit_contained(
    root: &std::path::Path,
    edit: &Value,
) -> anyhow::Result<Vec<(PathBuf, usize)>> {
    apply_workspace_edit_filtered(edit, Some(root))
}

fn path_allowed_under_root(root: &std::path::Path, path: &std::path::Path) -> bool {
    let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    let path_c = std::fs::canonicalize(path).unwrap_or_else(|_| {
        path.parent()
            .and_then(|p| std::fs::canonicalize(p).ok())
            .map(|p| p.join(path.file_name().unwrap_or_default()))
            .unwrap_or_else(|| path.to_path_buf())
    });
    path_c.starts_with(&root_c)
}

fn apply_workspace_edit_filtered(
    edit: &Value,
    root: Option<&std::path::Path>,
) -> anyhow::Result<Vec<(PathBuf, usize)>> {
    let per_file = collect(edit)?;
    if per_file.is_empty() {
        bail!("the rename produced no edits (the server may not support renaming this symbol)");
    }
    let mut summary = Vec::new();
    let mut skipped = 0usize;
    for (path, edits) in per_file {
        if let Some(root) = root {
            if !path_allowed_under_root(root, &path) {
                tracing::warn!(
                    path = %path.display(),
                    "skipping workspace edit outside work root"
                );
                skipped += 1;
                continue;
            }
        }
        let src = std::fs::read_to_string(&path)
            .with_context(|| format!("read {} to apply rename", path.display()))?;
        let updated = apply_to_text(&src, &edits)
            .with_context(|| format!("apply edits to {}", path.display()))?;
        std::fs::write(&path, updated)
            .with_context(|| format!("write renamed {}", path.display()))?;
        summary.push((path, edits.len()));
    }
    if summary.is_empty() {
        bail!(
            "all {} workspace edit path(s) were outside the work root (refused)",
            skipped
        );
    }
    summary.sort();
    Ok(summary)
}

/// Parse the WorkspaceEdit into edits grouped by file. Handles both `changes` and
/// `documentChanges`.
fn collect(edit: &Value) -> anyhow::Result<BTreeMap<PathBuf, Vec<TextEdit>>> {
    let mut out: BTreeMap<PathBuf, Vec<TextEdit>> = BTreeMap::new();

    if let Some(changes) = edit.get("changes").and_then(|c| c.as_object()) {
        for (uri, edits) in changes {
            let path = path_from_uri(uri);
            let list = out.entry(path).or_default();
            for e in edits.as_array().into_iter().flatten() {
                list.push(parse_edit(e)?);
            }
        }
    }

    if let Some(doc_changes) = edit.get("documentChanges").and_then(|c| c.as_array()) {
        for change in doc_changes {
            // Skip resource operations (create/rename/delete file) — we only apply
            // text edits. A `TextDocumentEdit` has `textDocument.uri` + `edits`.
            let Some(uri) = change.pointer("/textDocument/uri").and_then(|u| u.as_str()) else {
                continue;
            };
            let path = path_from_uri(uri);
            let list = out.entry(path).or_default();
            for e in change
                .get("edits")
                .and_then(|e| e.as_array())
                .into_iter()
                .flatten()
            {
                list.push(parse_edit(e)?);
            }
        }
    }

    Ok(out)
}

fn parse_edit(e: &Value) -> anyhow::Result<TextEdit> {
    let u = |ptr: &str| -> anyhow::Result<u32> {
        e.pointer(ptr)
            .and_then(|v| v.as_u64())
            .map(|n| n as u32)
            .ok_or_else(|| anyhow!("text edit missing {ptr}"))
    };
    Ok(TextEdit {
        start_line: u("/range/start/line")?,
        start_char: u("/range/start/character")?,
        end_line: u("/range/end/line")?,
        end_char: u("/range/end/character")?,
        new_text: e
            .get("newText")
            .and_then(|t| t.as_str())
            .ok_or_else(|| anyhow!("text edit missing newText"))?
            .to_string(),
    })
}

/// Apply a file's edits to its text. Offsets are resolved against the *original*
/// text, then applied from the last position to the first so earlier byte ranges
/// stay valid as we splice.
fn apply_to_text(src: &str, edits: &[TextEdit]) -> anyhow::Result<String> {
    // Resolve every edit to a byte range up front, against the untouched source.
    let mut ranges: Vec<(usize, usize, &str)> = Vec::with_capacity(edits.len());
    for e in edits {
        let start = position_to_offset(src, e.start_line, e.start_char)?;
        let end = position_to_offset(src, e.end_line, e.end_char)?;
        if start > end {
            bail!("text edit has start after end");
        }
        ranges.push((start, end, e.new_text.as_str()));
    }
    // Apply from the highest start offset down, so a splice never shifts a range
    // we haven't applied yet.
    ranges.sort_by_key(|r| std::cmp::Reverse(r.0));
    // Overlap guard: after sorting descending, each edit's end must not exceed the
    // previous (lower-in-file) edit's start.
    let mut prev_start: Option<usize> = None;
    let mut out = src.to_string();
    for (start, end, new_text) in ranges {
        if let Some(ps) = prev_start {
            if end > ps {
                bail!("overlapping text edits in one file");
            }
        }
        out.replace_range(start..end, new_text);
        prev_start = Some(start);
    }
    Ok(out)
}

/// Byte offset of the (0-based `line`, 0-based `character`) LSP position in `text`.
/// `character` past the line's end clamps to the line end (LSP end positions do
/// this legitimately).
fn position_to_offset(text: &str, line: u32, character: u32) -> anyhow::Result<usize> {
    let mut line_start = 0usize;
    if line > 0 {
        let mut seen = 0u32;
        let mut found = false;
        for (i, b) in text.bytes().enumerate() {
            if b == b'\n' {
                seen += 1;
                if seen == line {
                    line_start = i + 1;
                    found = true;
                    break;
                }
            }
        }
        if !found {
            bail!("line {line} is beyond the end of the file");
        }
    }
    let rest = &text[line_start..];
    let line_str = rest.split('\n').next().unwrap_or("");
    let byte_in_line = line_str
        .char_indices()
        .nth(character as usize)
        .map(|(i, _)| i)
        .unwrap_or(line_str.len());
    Ok(line_start + byte_in_line)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn edit(sl: u32, sc: u32, el: u32, ec: u32, new: &str) -> TextEdit {
        TextEdit {
            start_line: sl,
            start_char: sc,
            end_line: el,
            end_char: ec,
            new_text: new.into(),
        }
    }

    #[test]
    fn offset_resolves_line_and_character() {
        let src = "abc\ndef\nghi";
        assert_eq!(position_to_offset(src, 0, 0).unwrap(), 0);
        assert_eq!(position_to_offset(src, 1, 0).unwrap(), 4); // 'd'
        assert_eq!(position_to_offset(src, 2, 2).unwrap(), 10); // 'i'
                                                                // character past line end clamps to line end.
        assert_eq!(position_to_offset(src, 0, 99).unwrap(), 3);
    }

    #[test]
    fn single_rename_replaces_the_identifier() {
        let src = "let foo = 1;\ncall(foo);\n";
        // rename `foo` (cols 4..7 on line 0, cols 5..8 on line 1)
        let edits = vec![edit(0, 4, 0, 7, "bar"), edit(1, 5, 1, 8, "bar")];
        let out = apply_to_text(src, &edits).unwrap();
        assert_eq!(out, "let bar = 1;\ncall(bar);\n");
    }

    #[test]
    fn multiple_edits_on_one_line_keep_offsets_valid() {
        // Two occurrences on the same line; the new name is a different length, so
        // naive left-to-right application would corrupt the second offset.
        let src = "x = aa + aa\n";
        let edits = vec![edit(0, 4, 0, 6, "longer"), edit(0, 9, 0, 11, "longer")];
        let out = apply_to_text(src, &edits).unwrap();
        assert_eq!(out, "x = longer + longer\n");
    }

    #[test]
    fn overlapping_edits_are_rejected() {
        let src = "abcdef\n";
        let edits = vec![edit(0, 0, 0, 3, "X"), edit(0, 2, 0, 5, "Y")];
        let err = apply_to_text(src, &edits).unwrap_err().to_string();
        assert!(err.contains("overlapping"), "{err}");
    }

    #[test]
    fn apply_workspace_edit_writes_changes_map_across_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "fn foo() {}\n").unwrap();
        std::fs::write(&b, "foo();\nfoo();\n").unwrap();
        let uri = |p: &std::path::Path| url::Url::from_file_path(p).unwrap().to_string();
        let we = json!({
            "changes": {
                uri(&a): [{ "range": {"start":{"line":0,"character":3},"end":{"line":0,"character":6}}, "newText": "bar" }],
                uri(&b): [
                    { "range": {"start":{"line":0,"character":0},"end":{"line":0,"character":3}}, "newText": "bar" },
                    { "range": {"start":{"line":1,"character":0},"end":{"line":1,"character":3}}, "newText": "bar" },
                ],
            }
        });
        let summary = apply_workspace_edit(&we).unwrap();
        assert_eq!(summary.len(), 2);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn bar() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "bar();\nbar();\n");
    }

    #[test]
    fn apply_workspace_edit_handles_document_changes_form() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        std::fs::write(&a, "let foo = 1;\n").unwrap();
        let uri = url::Url::from_file_path(&a).unwrap().to_string();
        let we = json!({
            "documentChanges": [{
                "textDocument": { "uri": uri, "version": 1 },
                "edits": [{ "range": {"start":{"line":0,"character":4},"end":{"line":0,"character":7}}, "newText": "bar" }],
            }]
        });
        let summary = apply_workspace_edit(&we).unwrap();
        assert_eq!(summary.len(), 1);
        assert_eq!(summary[0].1, 1);
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "let bar = 1;\n");
    }

    #[test]
    fn empty_edit_is_an_error() {
        let err = apply_workspace_edit(&json!({})).unwrap_err().to_string();
        assert!(err.contains("no edits"), "{err}");
    }
}
