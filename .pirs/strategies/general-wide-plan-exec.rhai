// Task-neutral parallel dispatch: three read-only planners investigate the SAME
// task from different angles CONCURRENTLY, their plans are merged, then one fresh
// executor acts on the combined plan. Like `wide-plan-exec` but with no
// assumption about tests or targets — for any coding task where a single planning
// pass might miss something (unfamiliar code, cross-cutting change, ambiguous
// requirements).
//
//   pirs --strategy general-wide-plan-exec "make the config loader support env overrides"
//
// The `parallel` branches run at once (read-only by contract, so they can't race
// on the tree). `join: "concat"` stitches each plan under a `## Branch N` heading
// into the executor's {prev}. Three planners cost ~3x the planning tokens — use
// it when breadth is worth that, not for trivial edits.

#{
    name: "general-wide-plan-exec",
    persist: false,
    phases: [
        #{
            parallel: [
                #{
                    scope: "readonly",
                    system: "You are a senior engineer planning a change. Focus on \
                             the most DIRECT implementation: the smallest set of \
                             edits that satisfies the task. Investigate read-only \
                             and output a self-contained plan naming exact files \
                             and functions. Do NOT edit.",
                    prompt: "Plan the most direct implementation of this task.\n\n## Task\n{issue}\n\nOutput ONLY the plan.",
                },
                #{
                    scope: "readonly",
                    system: "You are a senior engineer planning a change. Focus on \
                             EDGE CASES and correctness: what inputs, states, or \
                             interactions could break, and how the change must \
                             handle them. Investigate read-only and output a \
                             self-contained plan naming exact files and functions. \
                             Do NOT edit.",
                    prompt: "Plan this task with an emphasis on edge cases and correctness.\n\n## Task\n{issue}\n\nOutput ONLY the plan.",
                },
                #{
                    scope: "readonly",
                    system: "You are a senior engineer planning a change. Focus on \
                             FIT with the existing codebase: conventions, existing \
                             abstractions to reuse, and consistency with nearby \
                             code. Investigate read-only and output a self-contained \
                             plan naming exact files and functions. Do NOT edit.",
                    prompt: "Plan this task to fit cleanly with the existing codebase.\n\n## Task\n{issue}\n\nOutput ONLY the plan.",
                },
            ],
            join: "concat",
        },
        #{
            scope: "full",
            system: "You are executing a change in a real repository, working from \
                     three independent plans of the SAME task (direct, edge-case, \
                     and codebase-fit angles). Reconcile them into the single best \
                     minimal change, make exactly those edits, and verify with the \
                     project's build/tests if present. Stop when the task is done.",
            prompt: "Execute this task using the merged plans below.\n\n\
                     ## Task\n{issue}\n\n\
                     ## Plans (three angles)\n{prev}\n\n\
                     Reconcile them, make the edits now, and verify.",
        },
    ],
}
