// Example pirs extension: custom tool, safety hook, and loop-behavior hooks.
// Drop this file in .pirs/extensions/ (project) or ~/.pirs/extensions/ (global).
//
// Message shape: maps like #{ role: "user", content: "text", timestamp: 0 }.
// Note: string interpolation uses backtick strings: `value = ${x}`

// --- custom tool -----------------------------------------------------------
register_tool("word_count", "Count words in a string", #{
    type: "object",
    properties: #{ text: #{ type: "string" } },
    required: ["text"]
});

fn tool_word_count(args) {
    let n = args.text.split(" ").filter(|w| w != "").len();
    `${n} words`
}

// --- tool policy -----------------------------------------------------------
fn on_tool_call(id, name, args) {
    if name == "bash" && args.command.contains("rm -rf") {
        return #{ block: true, reason: "destructive command rejected by policy" };
    }
    ()
}

// --- loop behavior ---------------------------------------------------------

// Rewrite the outgoing transcript before every LLM call.
fn on_context(messages) {
    state_set("turns", if state_has("turns") { state_get("turns") + 1 } else { 1 });
    messages
}

// Stop the run after 10 turns no matter what the model wants.
fn on_should_stop(info) {
    state_has("turns") && state_get("turns") >= 10
}

// Inject a steering message once, before the first LLM call.
fn on_steering() {
    if state_has("steered") {
        ()
    } else {
        state_set("steered", true);
        "Remember: keep answers short."
    }
}

// Observe loop events: agent_start, turn_end, tool_execution_end, ...
fn on_event(ty, data) {
    if ty == "tool_execution_end" && data.isError {
        print(`tool ${data.name} failed: ${data.text}`);
    }
}
