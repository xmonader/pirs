use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

/// Always-on tool protocol: no filler narration between tool calls.
pub const SILENT_TOOLS_RULE: &str = "Do not narrate what you are about to do. Either issue tool \
calls or state your conclusion. Speech between tool calls (\"let me search…\", \"now I'll \
read…\") is wasted — submit tools silently, then answer.";

pub fn build_system_prompt(cwd: &Path, tools: &[Arc<dyn AgentTool>]) -> String {
    build_system_prompt_with_map(cwd, tools, None, false)
}

/// Build the system prompt, optionally appending a PageRank repo-map sketch
/// and weak-model edit guidance.
pub fn build_system_prompt_with_map(
    cwd: &Path,
    tools: &[Arc<dyn AgentTool>],
    repo_map: Option<&str>,
    weak: bool,
) -> String {
    build_system_prompt_full(cwd, tools, repo_map, weak, None)
}

/// Full coding prompt: map + optional auto-recall block (no mandatory recall tool).
pub fn build_system_prompt_full(
    cwd: &Path,
    tools: &[Arc<dyn AgentTool>],
    repo_map: Option<&str>,
    weak: bool,
    auto_recall: Option<&str>,
) -> String {
    let mut prompt = String::new();
    prompt.push_str("You are an expert coding assistant operating inside pirs, a Rust port of the pi agent harness.\n\n");

    prompt.push_str("Available tools:\n");
    for tool in tools {
        if let Some(snippet) = tool.prompt_snippet() {
            prompt.push_str(&format!("- {snippet}\n"));
        } else {
            prompt.push_str(&format!("- {}: {}\n", tool.name(), tool.description()));
        }
    }

    let has_code_search = tools.iter().any(|t| t.name() == "code_search");
    let has_code_map = tools.iter().any(|t| t.name() == "code_map");
    let has_edit_block = tools.iter().any(|t| t.name() == "edit_block");

    prompt.push_str(
        "\nGuidelines:\n\
        - Be concise and direct.\n\
        - Show file paths when referencing code.\n\
        - Use read to inspect files, edit to make targeted changes, write for new files.\n",
    );
    prompt.push_str("- ");
    prompt.push_str(SILENT_TOOLS_RULE);
    prompt.push('\n');
    if has_edit_block {
        prompt.push_str(
            "- Prefer edit_block (SEARCH/REPLACE) when the change is a clear contiguous block; \
             use edit for multiple independent replacements in one file.\n",
        );
    }
    if has_code_search {
        prompt.push_str(
            "- To locate code, call code_search FIRST: one ranked call maps a symbol, \
             error string, or plain-language description of behavior to the most \
             relevant file:line hits. Read those hits directly. Only fall back to grep \
             for literal strings in non-code files or to confirm an exact match — do \
             not open a broad grep/read hunt when code_search would answer in one call.\n",
        );
    } else if has_code_map {
        prompt.push_str(
            "- To understand structure, use code_map (symbol/callers/callees/top/blast) \
             before blind grep. The <repo_map> sketch below (if present) lists top symbols.\n",
        );
    } else {
        prompt.push_str("- Use grep/find/ls to explore the codebase instead of guessing paths.\n");
    }
    prompt.push_str(
        "- Prefer the `project` tool for test/lint/typecheck/build/format when available; \
         use bash for one-off git/ops commands.\n\
        - **Office files** (.docx .pptx .xlsx .pdf .odt …): always use `read` — it extracts \
         text (never raw ZIP/binary). To create/edit, prefer the `office_document` tool \
         (create/update with text, rows, or slides). For layout-heavy work, open skill \
         `office-documents` and use python-docx / openpyxl / python-pptx via bash.\n",
    );

    if weak {
        prompt.push_str(
            "\nWeak-model mode:\n\
            - Work in small steps: one read or one edit, then verify.\n\
            - After every file change, run the project tests/build with bash.\n\
            - For edits, copy exact text from read output; include 2–3 surrounding lines so oldText is unique.\n\
            - If edit fails twice on the same file, re-read the full function or use edit_block / safe_edit.\n\
            - If a shell command fails (not found, bad flags, missing path), do NOT re-run it — try a different command or diagnose first.\n\
            - Do not claim success without test evidence.\n",
        );
    }

    // Multi-root work context (or single cwd) — paths may use //name/rel.
    let ctx = pirs_tools::current_work_context();
    if ctx.roots.len() > 1 {
        prompt.push_str(&ctx.prompt_section());
    } else {
        prompt.push_str(&format!("\nCurrent working directory: {}\n", cwd.display()));
    }

    // Durable user identity (same soul.md as pirs-claw) — keep harness/claw consistent.
    prompt.push_str(&pirs_skills::soul_prompt_section());

    // Soulforge-style auto-detected toolchain commands.
    prompt.push_str(&pirs_tools::detect_profile(cwd).prompt_section());

    if let Some(map) = repo_map {
        if !map.trim().is_empty() {
            prompt.push('\n');
            prompt.push_str(map);
            if !map.ends_with('\n') {
                prompt.push('\n');
            }
        }
    }

    if let Some(recall) = auto_recall {
        if !recall.trim().is_empty() {
            prompt.push('\n');
            prompt.push_str(recall);
            if !recall.ends_with('\n') {
                prompt.push('\n');
            }
        }
    }

    // Live harness features (autonomy, strategy, packs, caps) — not only tools.
    if let Some(rt) = crate::runtime_features::live() {
        prompt.push_str(&rt.format_llm());
    }

    prompt
}

/// Pure helper: non-empty map string means structural inject is ready for the session prefix.
pub fn map_inject_is_material(repo_map: Option<&str>) -> bool {
    repo_map
        .map(|m| !m.trim().is_empty() && m.contains("<repo_map>"))
        .unwrap_or(false)
}

pub fn read_project_context(cwd: &Path) -> Option<String> {
    let mut out = String::new();
    for name in ["AGENTS.md", "CLAUDE.md"] {
        let path = cwd.join(name);
        if let Ok(content) = std::fs::read_to_string(&path) {
            let truncated: String = content.chars().take(20_000).collect();
            out.push_str(&format!("\n<{name}>\n{truncated}\n</{name}>\n"));
        }
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn weak_guidance_present_when_flag_set() {
        let p = build_system_prompt_with_map(Path::new("."), &[], None, true);
        assert!(p.contains("Weak-model mode"));
        assert!(p.contains("test evidence"));
    }

    #[test]
    fn silent_tools_rule_in_coding_prompt() {
        let p = build_system_prompt_with_map(Path::new("."), &[], None, false);
        assert!(
            p.contains(SILENT_TOOLS_RULE) || p.contains("Do not narrate what you are about to do"),
            "silent-tool rule missing from prompt: {p}"
        );
        assert!(p.contains("Speech between tool calls") || p.contains("wasted"));
    }

    #[test]
    fn repo_map_appended() {
        let map = "<repo_map>\nsrc/a.rs:\n  fn foo\n</repo_map>\n";
        assert!(map_inject_is_material(Some(map)));
        assert!(!map_inject_is_material(None));
        assert!(!map_inject_is_material(Some("   ")));
        assert!(!map_inject_is_material(Some("not a map tag")));
        let p = build_system_prompt_with_map(Path::new("."), &[], Some(map), false);
        assert!(p.contains("<repo_map>"));
        assert!(p.contains("fn foo"));
        // Coding path uses full builder with map + optional recall.
        let full = build_system_prompt_full(Path::new("."), &[], Some(map), false, None);
        assert!(map_inject_is_material(Some(map)));
        assert!(full.contains("<repo_map>") && full.contains(SILENT_TOOLS_RULE));
    }

    #[test]
    fn auto_recall_block_appended_when_provided() {
        let recall = "<session_memory>\n- gotcha: path containment\n</session_memory>\n";
        let p = build_system_prompt_full(Path::new("."), &[], None, false, Some(recall));
        assert!(p.contains("<session_memory>"));
        assert!(p.contains("path containment"));
    }
}
