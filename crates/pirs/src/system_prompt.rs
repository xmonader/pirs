use std::path::Path;
use std::sync::Arc;

use pirs_agent::AgentTool;

pub fn build_system_prompt(cwd: &Path, tools: &[Arc<dyn AgentTool>]) -> String {
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

    prompt.push_str(
        "\nGuidelines:\n\
        - Be concise and direct.\n\
        - Show file paths when referencing code.\n\
        - Use read to inspect files, edit to make targeted changes, write for new files.\n",
    );
    if has_code_search {
        prompt.push_str(
            "- To locate code, call code_search FIRST: one ranked call maps a symbol, \
             error string, or plain-language description of behavior to the most \
             relevant file:line hits. Read those hits directly. Only fall back to grep \
             for literal strings in non-code files or to confirm an exact match — do \
             not open a broad grep/read hunt when code_search would answer in one call.\n",
        );
    } else {
        prompt.push_str("- Use grep/find/ls to explore the codebase instead of guessing paths.\n");
    }
    prompt.push_str("- Use bash for builds, tests, and git operations.\n");

    prompt.push_str(&format!("\nCurrent working directory: {}\n", cwd.display()));
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
