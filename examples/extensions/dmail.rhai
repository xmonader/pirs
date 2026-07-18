// dmail.rhai — model-initiated time travel (kimi-style D-Mail).
// dmail(note) folds the visible history back to a checkpoint and injects the
// note as context from "your past self". The real session log is untouched —
// the fold applies to what the model sees from here on.

register_tool("dmail", "Fold the conversation back to a checkpoint and continue from there with a note. Use to escape a failed approach or fold away huge reads you no longer need.", #{
    type: "object",
    properties: #{
        note: #{ type: "string" },
        turns_back: #{ type: "integer" }
    },
    required: ["note"]
});

fn on_event(ty, data) {
    if ty == "turn_end" {
        let snaps = if state_has("snaps") { state_get("snaps") } else { [] };
        snaps.push(state_get("msg_count") ?? 0);
        state_set("snaps", snaps);
    }
    ()
}

fn tool_dmail(args) {
    let snaps = if state_has("snaps") { state_get("snaps") } else { [] };
    let back = args.turns_back ?? 1;
    let target = if snaps.len() > back { snaps[snaps.len() - 1 - back] } else { 0 };
    state_set("fold_to", #{ turn: target, note: args.note });
    `History folded ${back} turn(s). The note is now visible to you.`
}

fn on_context(messages) {
    if !state_has("fold_to") {
        return messages;
    }
    let fold = state_get("fold_to");
    state_del("fold_to");
    state_set("msg_count", messages.len());
    let keep = if fold.turn < messages.len() {
        fold.turn
    } else {
        messages.len()
    };
    let head = messages.extract(0, keep);
    let note = #{
        role: "user",
        content: `[dmail from your future self — the next ${keep} message(s) were folded away]\n${fold.note}`,
        timestamp: 0
    };
    head.push(note);
    head
}
