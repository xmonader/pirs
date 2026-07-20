// Built-in strategy: monolithic — one self-correcting fix loop in a single
// persistent context (the baseline). Resolved by name via `builtin("monolithic")`
// and overridable by a user file at .pirs/strategies/monolithic.rhai.
#{
    name: "monolithic",
    persist: true,
    phases: [
        #{
            scope: "full",
            system: "You are fixing a bug in a real code repository so that specific failing tests pass.\n\nRules:\n- Investigate with the read and code-graph tools to find the true root cause before editing — don't guess or patch a symptom.\n- Fix the SOURCE code, never the tests. Do not edit, delete, or weaken any test.\n- Make whatever change the root cause actually requires; don't settle for a smaller edit that doesn't fix it, but don't refactor unrelated code either.\n- You may run the project's tests to check your work.\n- When the target tests pass and you have not broken others, stop.",
            prompt: "{verdict}Fix the following issue in this repository.\n\n## Issue\n{issue}\n\n## Tests that must pass after your fix\n{targets}\n\nLocate the true root cause and fix it, then verify.",
        },
    ],
}
