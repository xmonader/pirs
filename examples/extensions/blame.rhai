// blame.rhai — line-level provenance. When HEAD moves during a turn (the
// agent committed), annotate the new commit with (session, turn, model) via
// git notes. `pirs blame <file:line>` reads them back.
//
// Note keys are deliberately grep-friendly: pirs-session=, pirs-turn=,
// pirs-model=.

fn on_event(ty, data) {
    if ty != "turn_end" { return (); }
    let turn = if state_has("turn") { state_get("turn") } else { 0 };
    let turn = turn + 1;
    state_set("turn", turn);

    let head = exec("git rev-parse HEAD", 10);
    if head.code != 0 { return (); }
    let cur = head.output;
    cur.trim();  // rhai trim() mutates in place; assigning its result gives ()
    let prev = if state_has("head") { state_get("head") } else { "" };
    if cur == prev { return (); }
    state_set("head", cur);

    let note = `pirs-session=${session_id()} pirs-turn=${turn} pirs-model=${agent_model()}`;
    exec(`git notes add -f -m "${note}" ${cur}`, 10);
    ()
}
