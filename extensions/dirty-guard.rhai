// dirty-guard.rhai — commit pre-existing user WIP before AI edits a file
// (aider's dirty-file protection), so AI edits never entangle with user work
// and undo never eats user changes.
//
// Unlike every checkpoint/rollback pack in this catalog (rollback.rhai,
// stash-checkpoint.rhai, file-checkpoints.rhai), which deliberately avoid moving
// HEAD or touching the user's real branch, this one DOES: it's a genuine
// `git commit` onto whatever branch is checked out, with the message
// "wip: pre-ai-edit snapshot of <path>". That's the point (a real, durable
// safety commit the user's own `git log`/`reflog` will show), but it does
// mean these commits accumulate on the real branch and are yours to squash
// or leave, unlike the other packs' out-of-band refs.

fn on_tool_call(id, name, args) {
    if name != "edit" && name != "write" {
        return ();
    }
    let path = args.path ?? "";
    if path == "" {
        return ();
    }
    let status = exec(`git status --porcelain -- "${path}" 2>/dev/null`, 10);
    let s = status.output;
    s.trim();
    if s == "" {
        return ();
    }
    let key = "guarded:" + path;
    let guarded = if state_has("guarded") { state_get("guarded") } else { #{} };
    if guarded[key] ?? false {
        return ();
    }
    let commit = exec(
        `git add -- "${path}" && git -c user.email=pirs@localhost -c user.name=pirs commit -qm "wip: pre-ai-edit snapshot of ${path}" 2>&1`,
        20
    );
    if commit.code != 0 {
        return #{
            block: true,
            reason: `File '${path}' has uncommitted user changes and pre-edit commit failed: ${commit.output}. Commit or stash it yourself first.`
        };
    }
    guarded[key] = true;
    state_set("guarded", guarded);
    ()
}
