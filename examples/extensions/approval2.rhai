// approval2.rhai — semantic blast-radius: a sub-agent judges risky commands
// against the actual environment instead of matching fixed patterns.

const JUDGE_PROMPT = `You are a safety judge for shell commands. Given a command and environment info, answer APPROVE or DENY (one word) then a dash and a one-line reason.
Consider: does it delete data? overwrite uncommitted work? kill services? touch outside the project? install/run untrusted code?`;

fn on_tool_call(id, name, args) {
    if name != "bash" {
        return ();
    }
    let cmd = args.command ?? "";
    let risky = ["rm ", "rm -", "mv ", "dd ", "mkfs", "kill", "pkill", "chmod -R", "chown -R",
        "git push", "git reset --hard", "git clean", "curl ", "wget ", "> ", "pip install",
        "npm install", "apt ", "doas", "sudo"];
    let flagged = false;
    for p in risky {
        if cmd.contains(p) {
            flagged = true;
        }
    }
    if !flagged {
        return ();
    }
    let key = "ok:" + cmd;
    let approved = if state_has("approved") { state_get("approved") } else { #{} };
    if approved[key] ?? false {
        return ();
    }
    let env_info = exec("git status --porcelain 2>/dev/null | head -10; echo ---; git stash list 2>/dev/null | head -3", 5).output;
    let verdict = run_subagent(
        `${JUDGE_PROMPT}\n\nCOMMAND: ${cmd}\n\nENVIRONMENT:\n${env_info}\n\nVerdict:`
    );
    let v = verdict;
    v.trim();
    if v.starts_with("APPROVE") {
        approved[key] = true;
        state_set("approved", approved);
        return ();
    }
    #{
        block: true,
        reason: `safety judge denied '${cmd}': ${verdict}. Ask the user to approve explicitly, then retry.`
    }
}
