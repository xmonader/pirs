// instincts.rhai — learn (failure -> fix) pairs and steer away from repeats.

fn instincts_path() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/instincts.jsonl`
}

fn on_tool_result(id, name, result) {
    if result.isError {
        let text = result.text;
        if text.len() > 300 {
            text = text.sub(0, 300);
        }
        state_set("last_failure", #{ tool: name, text: text });
        return ();
    }
    if state_has("last_failure") {
        let f = state_get("last_failure");
        if f.tool == name {
            let entry = #{
                ts: now_millis(),
                tool: name,
                failure: f.text,
                fix: "succeeded on retry with different approach"
            };
            fs_append(instincts_path(), entry.to_json() + "\n");
            state_set("instinct_hit", f.text);
        }
        state_del("last_failure");
    }
    ()
}

fn on_steering() {
    if state_has("instinct_hit") {
        let text = state_get("instinct_hit");
        state_del("instinct_hit");
        return `Instinct recorded: this previously failed (${text}). Remember what worked.`;
    }
    ()
}
