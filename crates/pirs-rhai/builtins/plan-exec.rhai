// Built-in strategy: plan-exec — a read-only planner produces a self-contained
// fix plan, then a fresh executor (seeded only by that plan) applies it. Resolved
// by name via `builtin("plan-exec")`; a user file at .pirs/strategies/plan-exec.rhai
// overrides it.
#{
    name: "plan-exec",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            system: "You are a senior engineer producing a fix plan for a bug in a real repository.\n\n- Investigate with the read / search / code-graph tools to find the true root cause.\n- Do NOT edit any files. Output only a plan.\n- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your\n  investigation will execute it. Name the exact file(s) and function(s), state\n  the precise change, and keep it minimal.\n- Never propose editing, deleting, or weakening tests. The fix is in source.",
            prompt: "{verdict}Investigate and produce a self-contained fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (files, functions, exact changes). Do not edit.",
        },
        #{
            scope: "full",
            system: "You are fixing a bug in a real repository by executing an APPROVED plan.\n\n- Make the edits the plan specifies, and nothing more. Keep the change minimal.\n- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.\n- You may run the project's tests to check your work.\n- Follow the plan; if a step is clearly wrong, make the smallest correct fix.\n- When the target tests pass and you have not broken others, stop.",
            prompt: "Execute this approved fix plan in the repository.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Approved plan\n{prev}\n\nMake the edits now and verify.",
        },
    ],
}
