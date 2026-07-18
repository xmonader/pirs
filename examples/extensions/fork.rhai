// fork.rhai — cache-warm delegates: the sub-agent receives the current
// conversation prefix (qwen-code's fork pattern), so provider prefix-cache
// hits make it ~80% cheaper than a fresh-context delegate.

register_tool("fork", "Run a task in a sub-agent that inherits the current conversation (cache-warm). Use for related subtasks that benefit from shared context.", #{
    type: "object",
    properties: #{
        task: #{ type: "string" },
        model: #{ type: "string" }
    },
    required: ["task"]
});

fn tool_fork(args) {
    let history = state_get("history_text") ?? "";
    let model = args.model ?? "";
    let prompt = if history == "" {
        args.task
    } else {
        `${history}\n\n---\n[continuing the same work — full context above]\n${args.task}`
    };
    run_subagent(prompt, model)
}

fn on_context(messages) {
    let parts = [];
    for m in messages {
        let role = m.role;
        let text = m.content.to_string();
        if text.len() > 1500 {
            text = text.sub_string(0, 1500) + "...";
        }
        parts.push(`${role}: ${text}`);
    }
    let joined = str_join(parts, "\n\n");
    if joined.len() > 8000 {
        joined = joined.sub_string(joined.len() - 8000, 8000);
    }
    state_set("history_text", joined);
    messages
}
