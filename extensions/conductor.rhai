// conductor.rhai — strong-planner/weak-executor guidance pack (OPT-IN).
// Not in the default `packs: "*"` catalog — casual chat was hijacked by
// "[conductor mode]" on every turn. Load via profile packs: ["conductor"]
// or a custom profile when you want plan→delegate→verify orchestration.
//
// Pins conductor instructions so the main model plans, delegates execution
// steps with a model override, and verifies results.
//
// Doesn't register its own update_plan tool: an earlier version did, and if
// weak-model.rhai (which has a real, working one — pinned at the context
// tail via its own on_context, tied to its own state) was also loaded, tool
// registration is last-wins by name, so whichever pack's on_context read a
// state key its own tool_update_plan never got to populate would silently
// stop pinning anything — no error, just a quietly broken plan pin. Load
// weak-model.rhai alongside this pack if you want that tool; this one is
// just the planner/delegate/verify loop instructions.

const INSTRUCTIONS = "[conductor mode]";
const PLAN = `${INSTRUCTIONS}
You are the CONDUCTOR. Work in this loop:
1. Plan in numbered steps (use the update_plan tool if it's available; otherwise just write the steps out).
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
