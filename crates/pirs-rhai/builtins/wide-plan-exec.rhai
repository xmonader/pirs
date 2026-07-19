// Built-in strategy: wide-plan-exec — three read-only planners investigate the
// same bug from different angles in parallel, their plans are merged, then a
// fresh executor acts on the combined plan (the dispatch pattern). Resolved by
// name via `builtin("wide-plan-exec")`; overridable at
// .pirs/strategies/wide-plan-exec.rhai.
#{
    name: "wide-plan-exec",
    persist: false,
    phases: [
        #{
            parallel: [
                #{
                    scope: "readonly",
                    system: "You are a senior engineer producing a fix plan for a bug in a real repository.\n\n- Investigate with the read / search / code-graph tools to find the true root cause.\n- Do NOT edit any files. Output only a plan.\n- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your\n  investigation will execute it. Name the exact file(s) and function(s), state\n  the precise change, and keep it minimal.\n- Never propose editing, deleting, or weakening tests. The fix is in source.",
                    prompt: "Focus on the failing assertion itself: what value is produced vs expected, and which function computes it.\n\n{verdict}Investigate and produce a self-contained fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (files, functions, exact changes). Do not edit.",
                },
                #{
                    scope: "readonly",
                    system: "You are a senior engineer producing a fix plan for a bug in a real repository.\n\n- Investigate with the read / search / code-graph tools to find the true root cause.\n- Do NOT edit any files. Output only a plan.\n- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your\n  investigation will execute it. Name the exact file(s) and function(s), state\n  the precise change, and keep it minimal.\n- Never propose editing, deleting, or weakening tests. The fix is in source.",
                    prompt: "Focus on the most recently changed or most complex code path touched by the failing test.\n\n{verdict}Investigate and produce a self-contained fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (files, functions, exact changes). Do not edit.",
                },
                #{
                    scope: "readonly",
                    system: "You are a senior engineer producing a fix plan for a bug in a real repository.\n\n- Investigate with the read / search / code-graph tools to find the true root cause.\n- Do NOT edit any files. Output only a plan.\n- The plan must be SELF-CONTAINED: a fresh engineer with no memory of your\n  investigation will execute it. Name the exact file(s) and function(s), state\n  the precise change, and keep it minimal.\n- Never propose editing, deleting, or weakening tests. The fix is in source.",
                    prompt: "Focus on boundary/edge handling (empty, zero, off-by-one, sign) in the code under test.\n\n{verdict}Investigate and produce a self-contained fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (files, functions, exact changes). Do not edit.",
                },
            ],
            join: "concat",
        },
        #{
            scope: "full",
            system: "You are fixing a bug in a real repository by executing an APPROVED plan.\n\n- Make the edits the plan specifies, and nothing more. Keep the change minimal.\n- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.\n- You may run the project's tests to check your work.\n- Follow the plan; if a step is clearly wrong, make the smallest correct fix.\n- When the target tests pass and you have not broken others, stop.",
            prompt: "Execute this approved fix plan in the repository.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Approved plan\n{prev}\n\nMake the edits now and verify.",
        },
    ],
}
