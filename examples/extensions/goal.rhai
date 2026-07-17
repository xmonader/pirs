// goal.rhai — first-class session goals: set, pin, verify, persist.
// State lives in rhai scope, so the pin survives compaction.

fn goal_path() {
    let cwd = exec("pwd").output;
    cwd.trim();
    `${cwd}/.pirs/goal.md`
}

register_tool("set_goal", "Set the session goal. Pinned at the context tail, survives compaction, verified before the run ends.", #{
    type: "object",
    properties: #{ goal: #{ type: "string" } },
    required: ["goal"]
});

fn tool_set_goal(args) {
    state_set("goal", args.goal);
    state_del("goal_verified");
    fs_append(goal_path(), `# Goal\n\n${args.goal}\n`);
    "Goal set and pinned. It will be verified before the run ends."
}

fn on_context(messages) {
    if !state_has("goal") {
        return messages;
    }
    let goal = state_get("goal");
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with("[SESSION GOAL]"))
    );
    let pin = #{
        role: "user",
        content: `[SESSION GOAL]\n${goal}\n[do not lose sight of this; update with set_goal if it changes]`,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}

fn on_follow_up() {
    if !state_has("goal") || state_has("goal_verified") {
        return ();
    }
    state_set("goal_verified", true);
    [`Before finishing: check the SESSION GOAL above and either (a) confirm it is fully met with evidence, or (b) continue working on the unmet parts.`]
}

register_command("goal", "Show or set the session goal (usage: /goal [text])");

fn cmd_goal(args) {
    let raw = args;
    raw.trim();
    if raw != "" {
        return tool_set_goal(#{ goal: raw });
    }
    if state_has("goal") {
        `current goal: ${state_get("goal")}`
    } else {
        let persisted = fs_read(goal_path());
        if persisted == "" {
            "no goal set (use set_goal or /goal <text>)"
        } else {
            persisted
        }
    }
}

// Restore a persisted goal at session start.
fn on_steering() {
    if state_has("goal") || state_has("goal_offered") {
        return ();
    }
    state_set("goal_offered", true);
    let persisted = fs_read(goal_path());
    if persisted != "" {
        state_set("goal", persisted);
        return `A goal from a previous session was restored:\n${persisted}\nContinue working on it, or replace it with set_goal.`;
    }
    ()
}
