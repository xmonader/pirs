// spec-check.rhai — pin ACCEPTANCE.md and force a checklist pass before ending.

fn spec_path() {
    let cwd = exec("pwd").output;
    cwd.trim();
    `${cwd}/ACCEPTANCE.md`
}

fn on_context(messages) {
    let spec = fs_read(spec_path());
    if spec == "" {
        return messages;
    }
    let filtered = messages.filter(|m|
        !(m.role == "user" && m.content.to_string().starts_with("[acceptance criteria"))
    );
    let pin = #{
        role: "user",
        content: `[acceptance criteria — every item must hold before you finish]\n${spec}`,
        timestamp: 0
    };
    let idx = if filtered.len() > 0 { filtered.len() - 1 } else { 0 };
    filtered.insert(idx, pin);
    filtered
}

fn on_follow_up() {
    let spec = fs_read(spec_path());
    if spec == "" || state_has("spec_checked") {
        return ();
    }
    state_set("spec_checked", true);
    [`Before finishing: go through ACCEPTANCE.md item by item, verify each against the current state (run checks if needed), and report pass/fail per item. Fix any failures first.`]
}
