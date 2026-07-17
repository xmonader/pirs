// semantic-bookmarks.rhai — model-managed pinned notes at the context tail.

register_tool("bookmark", "Pin an important fact/excerpt at the context tail (max 5). Use for things you must not lose track of.", #{
    type: "object",
    properties: #{ text: #{ type: "string" } },
    required: ["text"]
});

fn tool_bookmark(args) {
    let marks = if state_has("marks") { state_get("marks") } else { [] };
    marks.push(args.text);
    if marks.len() > 5 {
        marks.shift();
    }
    state_set("marks", marks);
    `Bookmarked (${marks.len()}/5).`
}

fn on_context(messages) {
    if !state_has("marks") {
        return messages;
    }
    let marks = state_get("marks");
    if marks.len() == 0 {
        return messages;
    }
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with("[bookmarks]"))
    );
    let body = str_join(marks, "\n");
    let pin = #{
        role: "user",
        content: `[bookmarks]\n${body}`,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}
