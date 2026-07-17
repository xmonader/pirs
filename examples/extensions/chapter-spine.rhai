// chapter-spine.rhai — a weak-model writes one-line chapter titles; the spine
// keeps long sessions narratively coherent at tiny cost.

fn on_event(ty, data) {
    if ty != "turn_end" {
        return ();
    }
    let n = (state_get("turns") ?? 0) + 1;
    state_set("turns", n);
    if n % 5 != 0 || data.text == "" {
        return ();
    }
    let snippet = data.text;
    if snippet.len() > 400 {
        snippet = snippet.sub(0, 400);
    }
    let title = run_subagent(
        `Write a single-line chapter title (max 12 words) summarizing this progress, no preamble:\n${snippet}`
    );
    let spine = if state_has("spine") { state_get("spine") } else { [] };
    spine.push(`${n}. ${title}`);
    state_set("spine", spine);
}

fn on_context(messages) {
    if !state_has("spine") {
        return messages;
    }
    let spine = state_get("spine");
    if spine.len() == 0 {
        return messages;
    }
    let body = str_join(spine, "\n");
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with("[progress so far]"))
    );
    let pin = #{
        role: "user",
        content: `[progress so far]\n${body}`,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}
