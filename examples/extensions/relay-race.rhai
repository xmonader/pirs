// relay-race.rhai — draft -> critique -> finalize pipeline as a single tool.

register_tool("relay", "Run a task through a draft/critique/finalize relay of sub-agents. Returns the finalized output.", #{
    type: "object",
    properties: #{
        task: #{ type: "string" },
        model: #{ type: "string" }
    },
    required: ["task"]
});

fn tool_relay(args) {
    let model = args.model ?? "";
    let draft = run_subagent(`Produce a first draft for this task. Be complete but fast.\n\n${args.task}`, model);
    let critique = run_subagent(`Critique this draft rigorously: errors, gaps, edge cases. List concrete fixes.\n\nDRAFT:\n${draft}`, model);
    let final_out = run_subagent(`Produce the final version applying every valid fix from the critique.\n\nDRAFT:\n${draft}\n\nCRITIQUE:\n${critique}\n\nTASK:\n${args.task}`, model);
    final_out
}
