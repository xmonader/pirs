// approval.rhai — sensitive tool calls need explicit user approval.
// Flow: block with ticket -> model asks user -> user types "approve <id>"
// (or "approve all") -> model re-issues -> call passes.

fn is_sensitive(name, args) {
    let tools = ["bash"];
    if !tools.contains(name) {
        return false;
    }
    let patterns = [
        "rm ", "rm -", "mv ", "dd ", "mkfs", "chmod", "chown",
        "git push", "git commit", "git reset", "git clean",
        "pip install", "pip uninstall", "npm install", "npm uninstall",
        "apt ", "apt-get", "doas", "sudo", ">", "curl ", "wget ",
    ];
    let cmd = args.command ?? "";
    for p in patterns {
        if cmd.contains(p) {
            return true;
        }
    }
    false
}

fn on_tool_call(id, name, args) {
    if !is_sensitive(name, args) {
        return ();
    }
    if state_has("approve_all") && state_get("approve_all") {
        return ();
    }
    let key = name + "|" + args.to_json();
    let approved = if state_has("approved") { state_get("approved") } else { #{} };
    if (approved[key] ?? false) {
        return ();
    }
    let n = (state_get("ticket") ?? 0) + 1;
    state_set("ticket", n);
    let pending = if state_has("pending") { state_get("pending") } else { #{} };
    pending[`${n}`] = key;
    state_set("pending", pending);
    let shown = args.command ?? args.to_json();
    if shown.len() > 200 {
        shown = shown.sub_string(0, 200);
    }
    #{
        block: true,
        reason: `APPROVAL REQUIRED (#${n}): I want to run '${shown}'. Ask the user to reply 'approve ${n}' to allow it, or 'approve all' to allow all sensitive commands this session. Then re-issue the exact same call.`
    }
}

fn on_context(messages) {
    if messages.len() == 0 {
        return messages;
    }
    let last = messages[messages.len() - 1];
    if last.role != "user" {
        return messages;
    }
    let text = last.content.to_string();
    if text.starts_with("[") {
        return messages;
    }
    let trimmed = text;
    trimmed.trim();
    if trimmed == "approve all" {
        state_set("approve_all", true);
        return messages;
    }
    if trimmed.starts_with("approve ") {
        let id = trimmed.sub_string(8, trimmed.len() - 8);
        id.trim();
        let pending = if state_has("pending") { state_get("pending") } else { #{} };
        if pending.contains(id) {
            let key = pending[id];
            let approved = if state_has("approved") { state_get("approved") } else { #{} };
            approved[key] = true;
            state_set("approved", approved);
        }
    }
    messages
}
