// runs.rhai — durable run records: every run appends to ~/.pirs/runs/<ts>.jsonl;
// sessions that ended without agent_end are flagged interrupted on next start.

fn runs_dir() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/runs`
}

fn run_file() {
    if !state_has("run_file") {
        state_set("run_file", `${runs_dir()}/${now_millis()}.jsonl`);
    }
    state_get("run_file")
}

fn record(kind, text) {
    let t = text;
    if t.len() > 300 {
        t = t.sub_string(0, 300);
    }
    let entry = #{ ts: now_millis(), kind: kind, text: t };
    exec(`mkdir -p ${runs_dir()}`);
    fs_append(run_file(), entry.to_json() + "\n");
}

fn on_event(ty, data) {
    if ty == "agent_start" {
        record("agent_start", `model=${state_get("model") ?? "?"}`);
    }
    if ty == "turn_end" {
        record("turn_end", data.text);
    }
    if ty == "agent_end" {
        record("agent_end", `tokens=${data.totalTokens}`);
    }
    if ty == "tool_execution_end" && data.isError {
        record("tool_error", `${data.name}: ${data.text}`);
    }
    ()
}

fn on_steering() {
    if state_has("recovery_checked") {
        return ();
    }
    state_set("recovery_checked", true);
    let listing = exec(`ls -t ${runs_dir()} 2>/dev/null | head -5`, 5).output;
    let interrupted = [];
    for f in listing.split("\n") {
        if f == "" {
            continue;
        }
        let content = fs_read(`${runs_dir()}/${f}`);
        if content != "" && !content.contains("agent_end") {
            let last = content.split("\n").filter(|l| l != "").pop() ?? "";
            interrupted.push(`${f}: ${last}`);
        }
    }
    if interrupted.len() > 0 {
        return `Note: ${interrupted.len()} previous run(s) ended without completion (interrupted). Most recent:\n${interrupted[0]}\nContinue them or start fresh?`;
    }
    ()
}
