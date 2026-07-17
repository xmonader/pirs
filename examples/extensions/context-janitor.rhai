// context-janitor.rhai — shrink stale giant tool outputs in the outgoing
// context (keeps the tail; older than the last KEEP_RECENT messages).

// NOTE: rhai consts don't resolve inside nested closures — keep literals here.
fn shrink(text) {
    let lines = text.split("\n");
    if lines.len() <= 20 {
        return text;
    }
    let tail = lines.extract(lines.len() - 20, 20);
    let out = "[earlier output trimmed by context-janitor]\n";
    for line in tail {
        out = out + line + "\n";
    }
    out
}

fn on_context(messages) {
    let total = messages.len();
    messages.map(|m, idx| {
        if m.role != "toolResult" || idx >= total - 6 {
            return m;
        }
        if !m.contains("content") || m.content.type_of() != "array" {
            return m;
        }
        let blocks = m.content.map(|b| {
            if b.type_of() == "map" && b.contains("text") {
                b["text"] = shrink(b.text);
            }
            b
        });
        m["content"] = blocks;
        m
    })
}
