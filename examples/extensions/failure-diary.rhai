// failure-diary.rhai — log tool failures and pin recent pitfalls in context.

fn diary_path() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/failures.jsonl`
}

fn on_tool_result(id, name, result) {
    if !result.isError {
        return ();
    }
    let text = result.text;
    if text.len() > 300 {
        text = text.sub(0, 300);
    }
    let entry = #{ ts: now_millis(), tool: name, text: text };
    fs_append(diary_path(), entry.to_json() + "\n");
    let recent = if state_has("recent") { state_get("recent") } else { [] };
    recent.push(`${name}: ${text}`);
    if recent.len() > 3 {
        recent.shift();
    }
    state_set("recent", recent);
    ()
}

fn on_context(messages) {
    if !state_has("recent") {
        return messages;
    }
    let recent = state_get("recent");
    if recent.len() == 0 {
        return messages;
    }
    let body = str_join(recent, "\n");
    let pin = #{
        role: "user",
        content: `[known pitfalls this session — avoid repeating]\n${body}`,
        timestamp: 0
    };
    let idx = if messages.len() > 0 { messages.len() - 1 } else { 0 };
    messages.insert(idx, pin);
    messages
}
