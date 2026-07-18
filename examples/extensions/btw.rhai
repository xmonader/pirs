// btw.rhai — side questions that never enter the main history (qwen /btw).

register_command("btw", "Ask a side question (does not enter conversation history)");

fn cmd_btw(args) {
    let raw = args;
    raw.trim();
    if raw == "" {
        return "usage: /btw <question>";
    }
    let context = state_get("history_brief") ?? "";
    let prompt = if context == "" {
        raw
    } else {
        `Recent context (for reference only):\n${context}\n\nSide question (answer briefly, standalone): ${raw}`
    };
    let answer = run_subagent(prompt, "");
    `[btw] ${answer}`
}

fn on_context(messages) {
    let brief = "";
    let count = 0;
    for m in messages {
        if m.role == "user" && !m.content.to_string().starts_with("[") {
            let text = m.content.to_string();
            if text.len() > 200 {
                text = text.sub_string(0, 200) + "...";
            }
            brief = brief + text + " | ";
            count += 1;
        }
        if count >= 5 {
            break;
        }
    }
    state_set("history_brief", brief);
    messages
}
