// critic-arena.rhai — two models answer the same task; you judge.

register_tool("arena", "Run the same task on two models and return both answers side by side. Use for hard problems where a second opinion is cheap.", #{
    type: "object",
    properties: #{
        task: #{ type: "string" },
        model_a: #{ type: "string" },
        model_b: #{ type: "string" }
    },
    required: ["task"]
});

fn tool_arena(args) {
    let a = run_subagent(args.task, args.model_a ?? "");
    let b = run_subagent(args.task, args.model_b ?? "");
    let ma = if args.contains("model_a") { args.model_a } else { "default-a" };
    let mb = if args.contains("model_b") { args.model_b } else { "default-b" };
    `=== ${ma} ===\n${a}\n\n=== ${mb} ===\n${b}\n\nJudge both answers, point out where they disagree, and pick or merge.`
}
