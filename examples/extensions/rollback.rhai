// rollback.rhai — snapshot the worktree every turn (commit-tree, no touch to
// working files) and /undo <n> to rewind to turn n.

fn snapshot(msg) {
    let r = exec(`git add -A 2>/dev/null && tree=$(git write-tree) && oid=$(echo "${msg}" | git -c user.email=pirs@localhost -c user.name=pirs commit-tree $tree -p HEAD 2>/dev/null || git -c user.email=pirs@localhost -c user.name=pirs commit-tree $tree) && echo $oid`, 15);
    let oid = r.output;
    oid.trim();
    if r.code != 0 || oid == "" {
        return "";
    }
    let snaps = if state_has("snaps") { state_get("snaps") } else { [] };
    snaps.push(oid);
    state_set("snaps", snaps);
    let idx = snaps.len() - 1;
    exec(`git update-ref refs/pirs/turn-${idx} ${oid}`, 10);
    `${idx}`
}

fn on_event(ty, data) {
    if ty == "turn_end" && (state_get("dirty") ?? false) {
        state_del("dirty");
        let idx = snapshot(`turn snapshot`);
        if idx != "" {
            state_set("last_snap", idx);
        }
    }
    ()
}

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("dirty", true);
    }
    ()
}

register_command("undo", "Rewind working tree to snapshot N (default: last)");

fn cmd_undo(args) {
    let snaps = if state_has("snaps") { state_get("snaps") } else { [] };
    if snaps.len() == 0 {
        return "no snapshots this session";
    }
    let raw = args;
    raw.trim();
    let idx = if raw == "" { snaps.len() - 1 } else { raw.parse_json() };
    if idx < 0 || idx >= snaps.len() {
        return `usage: /undo <0..${snaps.len() - 1}> (you have ${snaps.len()} snapshots)`;
    }
    let r = exec(`git restore --source=${snaps[idx]} --worktree --staged .`, 15);
    if r.code != 0 {
        return `restore failed: ${r.output}`;
    }
    `restored to snapshot ${idx} (${snaps[idx]}). Files on disk rewound; history in the conversation is unchanged.`
}

register_command("snapshots", "List turn snapshots this session");

fn cmd_snapshots(args) {
    let snaps = if state_has("snaps") { state_get("snaps") } else { [] };
    if snaps.len() == 0 {
        return "no snapshots this session";
    }
    let out = "";
    for i in 0..snaps.len() {
        out = out + `${i} (refs/pirs/turn-${i}): ${snaps[i]}\n`;
    }
    out
}
