// red-team.rhai — after edits, a fresh-context adversary attacks the changes.

const ATTACK_PROMPT = "You are an adversarial reviewer. Attack this diff: find bugs, edge cases, race conditions, broken invariants. Be brutal and specific. If you find nothing wrong, say LGTM explicitly.";

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("needs_attack", true);
    }
    ()
}

fn on_follow_up() {
    if !state_has("needs_attack") || !state_get("needs_attack") {
        return ();
    }
    state_set("needs_attack", false);
    let diff = exec("git diff HEAD 2>/dev/null | head -c 12000", 15).output;
    let dtrim = diff;
    dtrim.trim();
    if dtrim == "" {
        return ();
    }
    let findings = run_subagent(`${ATTACK_PROMPT}\n\n${diff}`);
    if findings.contains("LGTM") {
        return ();
    }
    [`An adversarial reviewer found issues in your changes:\n${findings}\n\nFix the real ones (push back on wrong ones), then finish.`]
}
