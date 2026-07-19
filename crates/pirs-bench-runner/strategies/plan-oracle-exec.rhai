// The Amp "Oracle" pattern: plan → critic on a DIFFERENT model → execute.
//
// The plan and execute phases run on the run's default model (--model). The
// critic — the "second opinion" — is pinned to a stronger reasoning model via a
// per-phase `model` field. Run with a fast default so the contrast is real:
//
//   pirs-bench selftest --agent --provider deepseek --model deepseek-v4-flash \
//       --strategy-script this-file.rhai --trace run.jsonl ...
//
// The trace's phase.start events show each phase's model, so you can verify the
// critic ran on the oracle and measure whether the second opinion paid off.

#{
    name: "plan-oracle-exec",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            system: "You are a senior engineer producing a fix plan. Investigate with the \
                     read/search tools, find the root cause, and output a self-contained plan. \
                     Do NOT edit. Never touch tests.",
            prompt: "{verdict}Investigate and produce a self-contained fix plan.\n\n\
                     ## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\nOutput ONLY the plan.",
        },
        #{
            scope: "readonly",
            model: "deepseek-v4-pro",   // the Oracle: a different, stronger model
            system: "You are a senior reviewer giving a second opinion on a fix plan before it \
                     runs. Check it targets the real root cause, is minimal, and won't touch \
                     tests. You may read the code. Output the final plan to execute (corrected \
                     if needed).",
            prompt: "## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n\
                     ## Proposed plan\n{prev}\n\nReview and output the final plan.",
        },
        #{
            scope: "full",
            system: "You execute an APPROVED plan. Make the minimal edits it specifies. Fix \
                     source, never tests. Run the tests; when they pass and nothing else broke, stop.",
            prompt: "Execute this approved plan.\n\n## Issue\n{issue}\n\n## Tests\n{targets}\n\n\
                     ## Approved plan\n{prev}\n\nMake the edits now and verify.",
        },
    ],
}
