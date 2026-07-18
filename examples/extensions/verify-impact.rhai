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

fn on_tool_result(id, name, result) {
    if result.isError { return (); }
    if name != "edit" && name != "write" && name != "ast_edit" { return (); }
    let path = impacted_path(result);
    if path == "" { return (); }

    let names = graph_affected_tests(path);
    if names.len() == 0 { return (); }
    let run = if names.len() > 15 { names.extract(0, 15) } else { names };
    let cmd = `cargo test -- ${str_join(run, " ")}`;

    if state_has("dry_run") {
        state_set("last_impact_cmd", cmd);
        return ();
    }

    let r = exec(cmd, 180);
    let verdict = if r.code == 0 { "PASS" } else { "FAIL" };
    let out = r.output;
    let tail = if out.len() > 800 { "..." + out.sub_string(out.len() - 800, 800) } else { out };
    #{ text: result.text + `\n[impact ${verdict}] ${cmd}\n${tail}` }
}
