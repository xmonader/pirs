// Built-in strategy: plan-exec-weak — same plan→exec split as plan-exec, but
// prompts are written for weaker models: short sentences, one step at a time,
// exact paths, no open-ended refactoring. Prefer this with --weak / --profile weak.
#{
    name: "plan-exec-weak",
    persist: false,
    phases: [
        #{
            scope: "readonly",
            system: "You write a short fix plan for a bug.\n\nRules:\n- Use read/search/code_map only. Do NOT edit files.\n- Find the real root cause first.\n- Plan must be SELF-CONTAINED for a fresh engineer with no memory of your investigation.\n- Use a numbered list. Each step: exact file path, function/symbol name, and the precise change.\n- One step = one file when possible.\n- Do NOT change, delete, or weaken tests. Fix source only.\n- No refactors, no drive-by cleanups, no extra features.\n- Output ONLY the numbered plan.",
            prompt: "{verdict}Write a self-contained numbered fix plan.\n\n## Issue\n{issue}\n\n## Tests that must pass after the fix\n{targets}\n\nOutput ONLY the numbered plan (paths, functions, exact changes). Do not edit.",
        },
        #{
            scope: "full",
            system: "You apply an APPROVED plan to fix a bug.\n\nRules:\n- Do the plan steps in order. One edit, then check.\n- After each file change, run the listed tests (or the project test command).\n- Fix SOURCE only. Never edit, delete, or weaken tests.\n- If a step is wrong, make the smallest correct fix — do not redesign.\n- Prefer small unique edit blocks. If edit fails twice on the same file, re-read the full function and try a larger unique oldText, or use safe_edit if available.\n- When the target tests pass and you have not broken others, stop. Do not keep editing.",
            prompt: "Execute this approved plan. After edits, run tests. Stop when tests pass.\n\n## Issue\n{issue}\n\n## Tests that must pass\n{targets}\n\n## Approved plan\n{prev}\n\nMake the edits now, verify with tests, then stop.",
        },
    ],
}
