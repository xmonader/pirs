// Built-in strategy: plan-critic-exec — planner → critic gate → fresh executor.
// The critic vets (and if needed rewrites) the plan before any edit. Resolved by
// name via `builtin("plan-critic-exec")`; overridable at
// .pirs/strategies/plan-critic-exec.rhai.
#{
    name: "plan-critic-exec",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            system: "You are a senior engineer producing a fix plan for a bug in a real repository.\n\n- Investigate with the read / search / code-graph tools to find the true root cause.\n- Do NOT edit any files. Output only a plan.\n- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your\n  investigation will execute it. Name the exact file(s) and function(s), state\n  the precise change, and keep it minimal.\n- Never propose editing, deleting, or weakening tests. The fix is in source.",
            prompt: "{verdict}Investigate and produce a self-contained fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (files, functions, exact changes). Do not edit.",
        },
        #{
            scope: "readonly",
            system: "You are reviewing a proposed fix plan before it is executed.\n\nCheck it against the issue: does it target the real root cause, is it minimal,\ndoes it avoid touching tests, will it make the failing tests pass? You may read\nthe code to verify. If the plan is sound, output it unchanged. If it is flawed,\noutput a corrected, self-contained plan. Output ONLY the final plan to execute.",
            prompt: "## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Proposed plan\n{prev}\n\nReview and output the final plan to execute.",
        },
        #{
            scope: "full",
            system: "You are fixing a bug in a real repository by executing an APPROVED plan.\n\n- Make the edits the plan specifies, and nothing more. Keep the change minimal.\n- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.\n- You may run the project's tests to check your work.\n- Follow the plan; if a step is clearly wrong, make the smallest correct fix.\n- When the target tests pass and you have not broken others, stop.",
            prompt: "Execute this approved fix plan in the repository.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Approved plan\n{prev}\n\nMake the edits now and verify.",
        },
    ],
}
