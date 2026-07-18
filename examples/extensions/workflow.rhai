// workflow.rhai — a rerunnable multi-agent workflow: fan out reviews over a
// list of files in parallel, cache per-file verdicts (resume skips done work),
// then merge. Invoke as a tool: workflow(files, model?).
//
// The pattern this demonstrates:
//   parallel_map  — bounded-concurrency pipeline over items
//   cache_get/put — step-level result caching for reruns
//   inbox/spawn   — async sub-agents grouped in `jobs` by tag

register_tool("workflow", "Fan out reviewers over files in parallel (cached, resumable), then merge findings.", #{
    type: "object",
    properties: #{
        files: #{ type: "array" },
        model: #{ type: "string" }
    },
    required: ["files"]
});

fn review_file(file) {
    let cached = cache_get(`wf_review_${file}`);
    if cached != () {
        return cached;
    }
    let content = fs_read(file);
    let verdict = run_subagent(
        `Review this file for real bugs (correctness only, no style notes). One line per bug or 'CLEAN':\n\n${content}`
    );
    cache_put(`wf_review_${file}`, verdict);
    verdict
}

fn tool_workflow(args) {
    let model = args.model ?? "";
    state_set("workflow_name", "review-fanout");
    let verdicts = parallel_map(args.files, 4, "review_file", model);
    let out = "";
    for i in 0..args.files.len() {
        out = out + `## ${args.files[i]}\n${verdicts[i]}\n\n`;
    }
    let summary = run_subagent(
        `Merge these per-file review findings into one ranked list of real issues:\n\n${out}`,
        model
    );
    `=== merged review ===\n${summary}\n\n(per-file cache: rerun skips completed files)`
}
