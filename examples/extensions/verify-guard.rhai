// verify-guard.rhai — a passing verify command that ran ZERO tests does not
// count as verification (anti reward-hacking).

const TEST_PATTERNS = ["pytest", "cargo test", "npm test", "go test", "make test", "ctest"];

fn count_passed(text) {
    let nums = [];
    for line in text.split("\n") {
        if line.contains(" passed") {
            let parts = line.split(" ");
            for i in 0..parts.len() {
                if parts[i] == "passed" && i > 0 {
                    try {
                        nums.push(parts[i - 1].parse_int());
                    } catch (e) {}
                }
            }
        }
        if line.contains("no tests ran") || line.contains("0 passed") || line.contains("collected 0 items") {
            nums.push(0);
        }
    }
    if nums.len() == 0 {
        return ();
    }
    nums
}

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("dirty", true);
        return ();
    }
    if name != "bash" || result.isError || !state_has("dirty") {
        return ();
    }
    let cmd = state_get("last_cmd") ?? "";
    if cmd == "" {
        return ();
    }
    let is_test = false;
    for p in TEST_PATTERNS {
        if cmd.contains(p) {
            is_test = true;
        }
    }
    if !is_test {
        return ();
    }
    state_del("dirty");
    let counts = count_passed(result.text);
    if counts != () && counts.len() > 0 && counts.filter(|n| n > 0).len() == 0 {
        state_set("vacuous_green", cmd);
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
    if state_has("vacuous_green") {
        let cmd = state_get("vacuous_green");
        state_del("vacuous_green");
        return `Your verify command '${cmd}' reported ZERO passing tests. A green run with no tests is NOT verification. Write or run real tests that exercise your change, then verify again.`;
    }
    ()
}
