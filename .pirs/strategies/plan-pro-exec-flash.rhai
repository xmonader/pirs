// plan-pro-exec-flash — strong planner, cheap executor.
//
// The planner phase is pinned to deepseek-v4-pro via the per-phase `model`
// field; the executor phase omits `model` and falls back to whatever
// --model the run was given (deepseek-v4-flash). Same pattern as
// plan-oracle-exec.rhai, just applied to the planner instead of a critic.
//
//   pirs-bench solve <repo> --model deepseek-v4-flash --provider deepseek \
//       --strategy-script .pirs/strategies/plan-pro-exec-flash.rhai ...

#{
    name: "plan-pro-exec-flash",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            model: "deepseek-v4-pro",
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
