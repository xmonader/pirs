// session-handoff.rhai — carry context between sessions via .pirs/handoff.md.

fn handoff_path() {
    let cwd = exec("pwd").output;
    cwd.trim();
    `${cwd}/.pirs/handoff.md`
}

fn on_event(ty, data) {
    if ty == "agent_end" {
        return ();
    }
    if ty != "turn_end" {
        return ();
    }
    if data.text == "" {
        return ();
    }
    state_set("last_text", data.text);
}

fn on_context(messages) {
    if state_has("handoff_loaded") {
        return messages;
    }
    state_set("handoff_loaded", true);
    let content = fs_read(handoff_path());
    if content == "" {
        return messages;
    }
    let pin = #{
        role: "user",
        content: `[handoff from previous session]\n${content}\n[end of handoff]`,
        timestamp: 0
    };
    let idx = if messages.len() > 0 { messages.len() - 1 } else { 0 };
    messages.insert(idx, pin);
    messages
}

fn on_follow_up() {
    if state_has("last_text") && !state_has("handoff_written") {
        state_set("handoff_written", true);
        let text = state_get("last_text");
        fs_append(handoff_path(), `## session checkpoint\n${text}\n\n`);
    }
    ()
}
