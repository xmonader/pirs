// shadow-verify.rhai — re-run test commands and compare with claimed results.

const SAFE_TEST_PATTERNS = ["pytest", "cargo test", "npm test", "go test", "make test"];

fn on_tool_result(id, name, result) {
    if name != "bash" || result.isError {
        return ();
    }
    let cmd = state_get("last_cmd") ?? "";
    if cmd == "" {
        return ();
    }
    let matched = false;
    for p in SAFE_TEST_PATTERNS {
        if cmd.contains(p) {
            matched = true;
        }
    }
    if !matched {
        return ();
    }
    state_del("last_cmd");
    let rerun = exec(cmd, 120);
    if rerun.code != 0 {
        state_set("shadow_discrepancy", `claimed tests pass, but re-running '${cmd}' exited ${rerun.code}`);
    }
    ()
}

fn on_tool_call(id, name, args) {
    if name == "bash" {
        state_set("last_cmd", args.command);
    }
    ()
}

fn on_steering() {
    if state_has("shadow_discrepancy") {
        let msg = state_get("shadow_discrepancy");
        state_del("shadow_discrepancy");
        return `Shadow verification FAILED: ${msg}. Do not claim success.`;
    }
    ()
}
