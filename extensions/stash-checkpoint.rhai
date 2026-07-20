// stash-checkpoint.rhai — per-turn worktree snapshots via a "dangling
// stash" (cline's approach: `git stash create` anchored to a private ref).
// Rewinds files to a previous turn without touching conversation history,
// for git repos (see file-checkpoints.rhai for the non-git, per-file option).
//
// `git stash create` captures the worktree+index diff against HEAD in one
// command, without touching the index or working tree at all — no `git add
// -A` side effect that would leave the user's entire worktree staged after
// every turn. When a turn made no file changes, `git stash create` prints
// nothing; this pack tracks a `dirty` flag (via `on_tool_result`) to skip
// the git call entirely on read-only turns, falling back to snapshotting
// the current `HEAD` if `stash create` itself comes back empty (e.g.
// `dirty` was set by a tool call that ended up a no-op edit).
//
// Restore is `git stash apply`, which MERGES the snapshot's changes onto
// whatever is currently on disk (conflict markers on overlap) rather than
// unconditionally overwriting — uncommitted edits made since the snapshot
// are layered over, not discarded.
//
// Requires an existing git repo — there is no fallback (no shadow repo, no
// init) for a non-git working directory; the snapshot silently no-ops if
// `git rev-parse --is-inside-work-tree` fails.
//
// The resulting stash commit is genuinely "dangling" (unreachable, GC
// candidate) the instant `git stash create` returns it; `git update-ref`
// immediately anchors it under `refs/pirs/stash-turn-N` — a private
// namespace, invisible to `git stash list`/`git log`/branches, mirroring
// cline's own `refs/cline/checkpoints/...` pattern. These refs are never
// pruned automatically; delete them by hand (`git update-ref -d
// refs/pirs/stash-turn-N`) if they accumulate across a very long session.

fn snapshot() {
    if exec("git rev-parse --is-inside-work-tree", 5).code != 0 {
        return "";
    }
    let stash_sha = exec("git stash create", 10).output;
    stash_sha.trim();
    let sha = stash_sha;
    if sha == "" {
        let head = exec("git rev-parse HEAD", 5).output;
        head.trim();
        sha = head;
    }
    if sha == "" {
        return "";
    }
    let snaps = if state_has("stash_snaps") { state_get("stash_snaps") } else { [] };
    snaps.push(sha);
    state_set("stash_snaps", snaps);
    let idx = snaps.len() - 1;
    exec(`git update-ref refs/pirs/stash-turn-${idx} ${sha}`, 10);
    `${idx}`
}

fn on_event(ty, data) {
    if ty == "turn_end" && (state_get("dirty") ?? false) {
        state_del("dirty");
        snapshot();
    }
    ()
}

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("dirty", true);
    }
    ()
}

register_command("undo", "Restore files to a previous turn's dangling-stash snapshot (default: last). Merges onto disk via `git stash apply` rather than overwriting.");

fn cmd_undo(args) {
    let snaps = if state_has("stash_snaps") { state_get("stash_snaps") } else { [] };
    if snaps.len() == 0 {
        return "no snapshots this session";
    }
    let raw = args;
    raw.trim();
    let idx = if raw == "" { snaps.len() - 1 } else { raw.parse_json() };
    if idx < 0 || idx >= snaps.len() {
        return `usage: /undo <0..${snaps.len() - 1}> (you have ${snaps.len()} snapshots)`;
    }
    let sha = snaps[idx];
    let r = exec(`git stash apply ${sha}`, 15);
    if r.code != 0 {
        return `undo failed: ${r.output}`;
    }
    `restored snapshot ${idx} (${sha}) via git stash apply -- merged onto disk, so uncommitted changes made since then are layered over rather than discarded (check 'git status' for conflicts). History in the conversation is unchanged.`
}

register_command("snapshots", "List turn snapshots this session (dangling-stash checkpoints)");

fn cmd_snapshots(args) {
    let snaps = if state_has("stash_snaps") { state_get("stash_snaps") } else { [] };
    if snaps.len() == 0 {
        return "no snapshots this session";
    }
    let out = "";
    for i in 0..snaps.len() {
        out = out + `${i} (refs/pirs/stash-turn-${i}): ${snaps[i]}\n`;
    }
    out
}
