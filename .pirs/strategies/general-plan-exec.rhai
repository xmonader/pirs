// Task-neutral plan → execute loop. Unlike `plan-exec` (which is written for the
// bench's fix-a-failing-test workflow), this one makes no assumption about tests
// or targets — it works for any coding task: a feature, a refactor, a fix, a
// docs change.
//
//   pirs --strategy general-plan-exec "add a --json flag to the export command"
//
// Phase 1 investigates read-only and writes a self-contained plan; phase 2 starts
// fresh, seeded only by that plan (via {prev}), and makes the change. {verdict}
// is filled only when a verification gate re-runs the loop, empty otherwise.

#{
    name: "general-plan-exec",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            system: "You are a senior engineer planning a change in a real code \
                     repository. Investigate with the read/search/code-graph tools \
                     to understand exactly what the task needs and where. Do NOT \
                     edit any files. Output only a self-contained plan: name the \
                     exact file(s) and function(s) to touch and the precise, \
                     minimal change for each. A fresh engineer with no memory of \
                     your investigation must be able to execute it.",
            prompt: "{verdict}Investigate this task and produce a self-contained plan.\n\n\
                     ## Task\n{issue}\n\n\
                     Output ONLY the plan. Do not edit anything.",
        },
        #{
            scope: "full",
            system: "You are executing an APPROVED plan in a real repository. Make \
                     exactly the changes the plan specifies and nothing more; keep \
                     the change minimal. If the project has a build or tests, use \
                     them to verify your work. Stop when the task is done and you \
                     have not broken anything.",
            prompt: "Execute this approved plan.\n\n\
                     ## Task\n{issue}\n\n\
                     ## Approved plan\n{prev}\n\n\
                     Make the edits now and verify.",
        },
    ],
}
