//! Aider-style SEARCH/REPLACE edit format for weaker models.
//!
//! Models often emit fence-delimited blocks more reliably than nested JSON
//! `oldText`/`newText` objects. This tool accepts either structured fields or
//! a classic:
//! ```text
//! <<<<<<< SEARCH
//! old lines
//! =======
//! new lines
//! >>>>>>> REPLACE
//! ```

use std::path::PathBuf;

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::edit::apply_one_replacement;
use crate::paths;

#[derive(Deserialize, JsonSchema)]
struct EditBlockArgs {
    /// Path to the file to edit
    path: String,
    /// Full SEARCH/REPLACE block (optional if search+replace provided)
    block: Option<String>,
    /// Text text (optional if block provided)
    search: Option<String>,
    /// Replacement text (optional if block provided)
    replace: Option<String>,
}

pub struct EditBlockTool {
    cwd: PathBuf,
}

impl EditBlockTool {
    pub fn new(cwd: PathBuf) -> Self {
        EditBlockTool { cwd }
    }
}

#[async_trait]
impl AgentTool for EditBlockTool {
    fn name(&self) -> &str {
        "edit_block"
    }

    fn description(&self) -> &str {
        "Apply one SEARCH/REPLACE edit to a file (aider-style). Prefer this over \
         edit when making a single contiguous change — weaker models match this \
         format more reliably. Provide either `search`+`replace` fields, or a \
         `block` string containing <<<<<<< SEARCH / ======= / >>>>>>> REPLACE."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(EditBlockArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "edit_block: one SEARCH/REPLACE change (aider format; best for weak models)",
        )
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: EditBlockArgs = serde_json::from_value(ctx.args)?;
        let (search, replace) = resolve_search_replace(&args)?;
        let path = paths::resolve_contained(&self.cwd, &args.path)?;
        let _mutation_guard = crate::filelock::lock(&path).await;
        let raw =
            std::fs::read(&path).with_context(|| format!("failed to read {}", path.display()))?;
        let original = String::from_utf8_lossy(&raw).into_owned();
        let new_content = apply_one_replacement(&original, &search, &replace)?;
        std::fs::write(&path, new_content.as_bytes())
            .with_context(|| format!("failed to write {}", path.display()))?;

        let body_old = original.replace("\r\n", "\n");
        let body_new = new_content.replace("\r\n", "\n");
        // Drop BOM for patch display if present.
        let body_old = body_old.trim_start_matches('\u{feff}');
        let body_new = body_new.trim_start_matches('\u{feff}');
        let patch = similar::TextDiff::from_lines(body_old, body_new)
            .unified_diff()
            .context_radius(3)
            .header(&format!("a/{}", args.path), &format!("b/{}", args.path))
            .to_string();

        Ok(ToolOutput::text(format!(
            "Successfully applied SEARCH/REPLACE in {}",
            path.display()
        ))
        .with_details(json!({
            "path": args.path,
            "patch": patch,
        })))
    }
}

fn resolve_search_replace(args: &EditBlockArgs) -> anyhow::Result<(String, String)> {
    if let (Some(s), Some(r)) = (&args.search, &args.replace) {
        if s.is_empty() {
            bail!("search must not be empty");
        }
        return Ok((s.clone(), r.clone()));
    }
    let Some(block) = &args.block else {
        bail!("provide either search+replace, or a block with SEARCH/REPLACE markers");
    };
    parse_search_replace_block(block)
}

/// Parse one or more SEARCH/REPLACE blocks; returns the first pair.
pub fn parse_search_replace_block(block: &str) -> anyhow::Result<(String, String)> {
    let lines: Vec<&str> = block.lines().collect();
    let mut i = 0usize;
    while i < lines.len() {
        let t = lines[i].trim();
        if is_search_head(t) {
            i += 1;
            let mut search = String::new();
            while i < lines.len() && !is_divider(lines[i].trim()) {
                if is_replace_tail(lines[i].trim()) {
                    bail!(
                        "malformed block: found REPLACE before ======= divider. \
                         Use:\n<<<<<<< SEARCH\n...old...\n=======\n...new...\n>>>>>>> REPLACE"
                    );
                }
                if !search.is_empty() {
                    search.push('\n');
                }
                search.push_str(lines[i]);
                i += 1;
            }
            if i >= lines.len() || !is_divider(lines[i].trim()) {
                bail!("malformed block: missing ======= divider after SEARCH");
            }
            i += 1; // skip divider
            let mut replace = String::new();
            while i < lines.len() && !is_replace_tail(lines[i].trim()) {
                if is_search_head(lines[i].trim()) {
                    break;
                }
                if !replace.is_empty() {
                    replace.push('\n');
                }
                replace.push_str(lines[i]);
                i += 1;
            }
            if search.is_empty() {
                bail!("SEARCH section is empty");
            }
            return Ok((search, replace));
        }
        i += 1;
    }
    bail!(
        "no SEARCH/REPLACE block found. Expected:\n\
         <<<<<<< SEARCH\n\
         ...exact old lines...\n\
         =======\n\
         ...new lines...\n\
         >>>>>>> REPLACE"
    )
}

fn is_search_head(t: &str) -> bool {
    t.starts_with("<<<<<<<") && t.to_ascii_uppercase().contains("SEARCH")
}

fn is_divider(t: &str) -> bool {
    t.starts_with("=======")
}

fn is_replace_tail(t: &str) -> bool {
    t.starts_with(">>>>>>>") && t.to_ascii_uppercase().contains("REPLACE")
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn parses_classic_block() {
        let block = "\
<<<<<<< SEARCH
a - b
=======
a + b
>>>>>>> REPLACE
";
        let (s, r) = parse_search_replace_block(block).unwrap();
        assert_eq!(s, "a - b");
        assert_eq!(r, "a + b");
    }

    #[test]
    fn rejects_missing_divider() {
        let block = "<<<<<<< SEARCH\nfoo\n>>>>>>> REPLACE\n";
        assert!(parse_search_replace_block(block).is_err());
    }

    #[tokio::test]
    async fn applies_block_to_file() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("f.rs");
        std::fs::write(&path, "fn add(a: i32, b: i32) -> i32 {\n    a - b\n}\n").unwrap();
        let tool = EditBlockTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: json!({
                    "path": "f.rs",
                    "block": "<<<<<<< SEARCH\n    a - b\n=======\n    a + b\n>>>>>>> REPLACE\n"
                }),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(out.content[0].as_text().unwrap().contains("Successfully"));
        let body = std::fs::read_to_string(&path).unwrap();
        assert!(body.contains("a + b"));
        assert!(!body.contains("a - b"));
    }
}
