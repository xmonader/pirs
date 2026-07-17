// repo-pulse.rhai — keep a fresh repo-state pin (branch, dirty files) so the
// model never works from stale tree information.

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("repo_dirty", true);
    }
    ()
}

fn pulse() {
    let branch = exec("git rev-parse --abbrev-ref HEAD 2>/dev/null", 5).output;
    branch.trim();
    let dirty = exec("git status --porcelain 2>/dev/null | head -20", 5).output;
    if branch == "" {
        return "";
    }
    `[repo state] branch: ${branch}\nchanged files:\n${dirty}`
}

fn on_context(messages) {
    if !state_has("repo_dirty") {
        return messages;
    }
    state_del("repo_dirty");
    let body = pulse();
    if body == "" {
        return messages;
    }
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with("[repo state]"))
    );
    let pin = #{ role: "user", content: body, timestamp: 0 };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}
