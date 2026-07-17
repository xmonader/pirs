// reviewer.rhai — after file edits, force a review pass before the run ends.

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("needs_review", true);
    }
    ()
}

fn on_follow_up() {
    if state_has("needs_review") && state_get("needs_review") {
        state_set("needs_review", false);
        return ["Before finishing: review your changes (git diff if this is a git repo, otherwise re-read the files). Look for bugs, edge cases, and inconsistencies with the surrounding code. Fix anything you find, then summarize what changed."];
    }
    ()
}
