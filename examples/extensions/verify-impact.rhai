// verify-impact.rhai — graph-scoped verification. After a successful
// edit/write/ast_edit, ask the code graph which tests directly call the
// edited symbols (or live in the edited file) and run just those:
// `cargo test -- name1 name2 ...`. The outcome is appended to the tool
// result so the model sees it immediately.
//
// Set state_set("dry_run", true) to only record the command (testing).

fn impacted_path(result) {
    if "details" in result && "path" in result.details {
        return result.details.path;
    }
    let text = result.text;
    for sep in [" in ", " to "] {
        if text.contains(sep) {
            let parts = text.split(sep);
            let p = parts[parts.len() - 1];
            p.trim();
            return p;
        }
    }
    ""
}

// Decide what to run for an edited file. Fail OPEN: if the graph can't tie the
// file to any test (new / unindexed / stale index), run the FULL suite rather
// than silently passing — a verification gate that quietly skips is worse than
// none. Returns #{ cmd, scoped }.
fn impact_plan(path) {
    let names = graph_affected_tests(path);
    let scoped = names.len() > 0;
    let cmd = if scoped {
        let run = if names.len() > 15 { names.extract(0, 15) } else { names };
        `cargo test -- ${str_join(run, " ")}`
    } else {
        "cargo test"
    };
    #{ cmd: cmd, scoped: scoped }
}

fn on_tool_result(id, name, result) {
    if result.isError { return (); }
    if name != "edit" && name != "write" && name != "ast_edit" { return (); }
    let path = impacted_path(result);
    if path == "" { return (); }

    let plan = impact_plan(path);
    let scope_tag = if plan.scoped { "impact" } else { "impact FULL (no graph-reachable tests)" };

    if state_has("dry_run") {
        state_set("last_impact_cmd", plan.cmd);
        return ();
    }

    let r = exec(plan.cmd, if plan.scoped { 180 } else { 600 });
    let verdict = if r.code == 0 { "PASS" } else { "FAIL" };
    let out = r.output;
    let tail = if out.len() > 800 { "..." + out.sub_string(out.len() - 800, 800) } else { out };
    #{ text: result.text + `\n[${scope_tag} ${verdict}] ${plan.cmd}\n${tail}` }
}
