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
        "Make exact string replacements in a file. Each oldText must match exactly one location; edits are validated against the original file and must not overlap."
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

        let (body, bom, crlf) = normalize_file(&original);
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
        let mut edited = body.clone();
        for &i in order.iter().rev() {
            let (start, end, new) = &spans[i];
            let (start, end) = (*start, *end);
            let mut replacement = new.clone();
            if end > start && body[..end].ends_with('\n') && !replacement.ends_with('\n') {
                replacement.push('\n');
            }
            edited.replace_range(start..end, &replacement);
        }

        let mut out = String::new();
        if bom {
            out.push('\u{feff}');
        }
        if crlf {
            out.push_str(&edited.replace('\n', "\r\n"));
        } else {
            out.push_str(&edited);
        }
        std::fs::write(&path, out.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;

        let first_changed = first_changed_line(&body, &edited);
        let patch = unified_patch(&body, &edited, &args.path);
        Ok(ToolOutput::text(format!(
            "Successfully replaced {} block(s) in {}",
            spans.len(),
            path.display()
        ))
        .with_details(json!({
            "patch": patch,
            "firstChangedLine": first_changed,
        })))
    }
}

fn normalize_file(content: &str) -> (String, bool, bool) {
    let bom = content.starts_with('\u{feff}');
    let without_bom = if bom { &content[3..] } else { content };
    let crlf = without_bom.contains("\r\n");
    let body = if crlf {
        without_bom.replace("\r\n", "\n")
    } else {
        without_bom.to_string()
    };
    (body, bom, crlf)
}

fn locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    let matches: Vec<usize> = content.match_indices(old_text).map(|(i, _)| i).collect();
    match matches.len() {
        1 => return Some((matches[0], matches[0] + old_text.len())),
        n if n > 1 => return None,
        _ => {}
    }
    fuzzy_locate(content, old_text)
}

fn not_found_error(content: &str, old_text: &str) -> anyhow::Error {
    let exact = content.match_indices(old_text).count();
    if exact > 1 {
        anyhow::anyhow!(
            "oldText occurs {exact} times in the file; it must be unique. Add more surrounding context."
        )
    } else {
        anyhow::anyhow!(
            "oldText not found in the file. Check whitespace, indentation, and that the file has not changed."
        )
    }
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

fn fuzzy_locate(content: &str, old_text: &str) -> Option<(usize, usize)> {
    let content_lines: Vec<&str> = content.lines().collect();
    let old_lines: Vec<String> = old_text.lines().map(fuzzy_normalize).collect();
    if old_lines.is_empty() || old_lines.len() > content_lines.len() {
        return None;
    }
    let norm_content: Vec<String> = content_lines.iter().map(|l| fuzzy_normalize(l)).collect();

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
    fn normalize_file_crlf() {
        let (body, bom, crlf) = normalize_file("a\r\nb\r\n");
        assert_eq!(body, "a\nb\n");
        assert!(!bom);
        assert!(crlf);
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
}
