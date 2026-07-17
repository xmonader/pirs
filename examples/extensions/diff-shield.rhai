// diff-shield.rhai — merge consecutive same-tool results to compress context.

fn on_context(messages) {
    let out = [];
    for m in messages {
        if out.len() > 0 && m.role == "toolResult" {
            let prev = out[out.len() - 1];
            if prev.role == "toolResult"
                && prev.toolName == m.toolName
                && prev.content.type_of() == "array" && m.content.type_of() == "array" {
                let a = prev.content[0].text ?? "";
                let b = m.content[0].text ?? "";
                let merged = a + "\n---\n" + b;
                if merged.len() < 4000 {
                    let first = prev.content[0];
                    first["text"] = merged;
                    prev["content"] = [first];
                    out[out.len() - 1] = prev;
                    continue;
                }
            }
        }
        out.push(m);
    }
    out
}
