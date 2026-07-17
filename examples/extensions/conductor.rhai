// conductor.rhai — strong-planner/weak-executor guidance pack.
// Pins conductor instructions so the main model plans, delegates execution
// steps with a model override, and verifies results.

const INSTRUCTIONS = "[conductor mode]";
const PLAN = `${INSTRUCTIONS}
You are the CONDUCTOR. Work in this loop:
1. Plan with update_plan (small numbered steps).
2. Execute steps by delegating to sub-agents via the delegate tool — pass model explicitly when a cheaper model suffices (simple file writes, searches, boilerplate).
3. Verify every delegated artifact yourself (read the files, run builds/tests with bash) before marking a step done.
4. Only synthesize the final answer after all steps pass verification.`;

fn on_context(messages) {
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with(INSTRUCTIONS))
    );
    let pin = #{
        role: "user",
        content: PLAN,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}

register_tool("update_plan", "Set or update the working plan (numbered steps).", #{
    type: "object",
    properties: #{ plan: #{ type: "string" } },
    required: ["plan"]
});

fn tool_update_plan(args) {
    state_set("plan", args.plan);
    "Plan recorded."
}
