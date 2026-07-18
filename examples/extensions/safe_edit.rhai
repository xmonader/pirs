// safe_edit.rhai — editor-mode edits (aider's editor-diff): a narrow prompt
// that only produces the new file content. Better for weak models than the
// instruction-following edit tool, because there's nothing to misunderstand.

const EDITOR_PROMPT = `You are an editor model. You output ONLY the new full content of the file after applying the instruction. No explanation, no markdown fences, no commentary. Preserve everything not touched by the instruction exactly.`;

register_tool("safe_edit", "Rewrite a file per an instruction using the editor model (narrow prompt; best for weak models or big refactors). Returns a diff summary.", #{
    type: "object",
    properties: #{
        path: #{ type: "string" },
        instruction: #{ type: "string" },
        model: #{ type: "string" }
    },
    required: ["path", "instruction"]
});

fn tool_safe_edit(args) {
    let content = fs_read(args.path);
    if content == "" {
        return `file not found or empty: ${args.path}`;
    }
    let model = args.model ?? "";
    let new_content = run_subagent(
        `${EDITOR_PROMPT}\n\nFILE: ${args.path}\n\nCONTENT:\n${content}\n\nINSTRUCTION: ${args.instruction}\n\nNEW CONTENT:`,
        model
    );
    let n = new_content;
    n.trim();
    if n == "" || n == content.trim() {
        return "safe_edit made no change";
    }
    if n.starts_with("```") {
        let lines = n.split("\n");
        let body = lines.extract(1, lines.len() - 2);
        n = str_join(body, "\n");
    }
    fs_write(args.path, n + "\n");
    let before = content.split("\n").len();
    let after = n.split("\n").len();
    `safe_edit rewrote ${args.path} (${before} -> ${after} lines)`
}
