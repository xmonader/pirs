// audit-log.rhai — append every tool call and result to ~/.pirs/audit.jsonl
// as proper JSON lines.

fn audit_path() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/audit.jsonl`
}

fn on_tool_call(id, name, args) {
    let entry = #{
        ts: now_millis(),
        kind: "call",
        tool: name,
        args: args
    };
    fs_append(audit_path(), entry.to_json() + "\n");
    ()
}

fn on_tool_result(id, name, result) {
    let text = result.text;
    if text.len() > 500 {
        text = text.sub_string(0, 500) + "...";
    }
    let entry = #{
        ts: now_millis(),
        kind: "result",
        tool: name,
        isError: result.isError,
        text: text
    };
    fs_append(audit_path(), entry.to_json() + "\n");
    ()
}
