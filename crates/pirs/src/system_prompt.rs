use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

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
         use bash for one-off git/ops commands.\n",
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

    prompt
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
    fn repo_map_appended() {
        let p = build_system_prompt_with_map(
            Path::new("."),
            &[],
            Some("<repo_map>\nsrc/a.rs:\n  fn foo\n</repo_map>\n"),
            false,
        );
        assert!(p.contains("<repo_map>"));
        assert!(p.contains("fn foo"));
    }
}
