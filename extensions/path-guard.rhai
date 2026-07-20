// path-guard.rhai — flags bash commands that write, delete, or change
// permissions on paths outside the working directory, plus `find`'s
// `-exec`/`-delete` (which can touch an unbounded set of matched paths no
// static check can fully account for). Modeled on mistral-vibe's bash
// guardrails, which parse the command with tree-sitter-bash to reason about
// argv/paths structurally; pirs has no bash grammar (only rust/py/ts/go, in
// pirs-graph, for code search) and adding one would be a core change, so
// this is naive whitespace tokenization instead — good enough to catch the
// common case, not a real parser. It will both over- and under-match on
// sufficiently exotic quoting, variable expansion, or command substitution.
// Treat it as a speed bump, not a security boundary: sandbox.rhai's
// OS-level filesystem confinement is the actual boundary, and this pack is
// what still helps when that isn't (or can't be) active, or as a second,
// independent layer when it is.
//
// Unlike guardrails.rhai's fixed denylist of exact destructive patterns,
// this reasons about *where* a sensitive command's targets are relative to
// cwd, so it also catches things nobody hardcoded a pattern for — `chmod
// -R 777 /some/other/project`, `rm -rf ~/Downloads`, `cp secrets.txt
// /tmp/../etc/`. Composes cleanly with guardrails.rhai and sandbox.rhai:
// pirs runs every loaded pack's `on_tool_call`, in load order, and the
// first one that blocks wins (see pirs-rhai's run_on_tool_call).
//
// Project-specific exceptions: add a line to .pirs/path-guard-allow.txt
// (one substring per line, `#` comments allowed) — any command containing
// that substring is let through without evaluation. Deliberately a plain
// substring allowlist, not a path-equality check, so one line can cover a
// family of related commands (e.g. a shared build-output directory).

const SENSITIVE_VERBS = [
    "rm", "mv", "cp", "chmod", "chown", "dd", "tee", "truncate", "shred", "rsync",
];

fn read_lines(path) {
    let content = fs_read(path);
    let out = [];
    for line in content.split("\n") {
        let l = line;
        l.trim();
        if l != "" && !l.starts_with("#") {
            out.push(l);
        }
    }
    out
}

// A token counts as "outside cwd" if it's a home-relative path (~...), a
// parent-traversal (..), or an absolute path that isn't a prefix-match under
// cwd. Relative paths (no leading /, ~, or ..) are assumed to resolve inside
// cwd, since bash's own cwd is what pirs launched the command from.
fn is_outside_cwd(token, cwd) {
    if token.starts_with("~") {
        return true;
    }
    if token == ".." || token.starts_with("../") || token.contains("/../") {
        return true;
    }
    if token.starts_with("/") {
        return !token.starts_with(cwd);
    }
    false
}

fn on_tool_call(id, name, args) {
    if name != "bash" {
        return ();
    }
    let cmd = args.command;
    let cwd = exec("pwd", 5).output;
    cwd.trim();

    for a in read_lines(`${cwd}/.pirs/path-guard-allow.txt`) {
        if cmd.contains(a) {
            return ();
        }
    }

    if cmd.contains("find ") && (cmd.contains("-exec") || cmd.contains("-delete")) {
        return #{
            block: true,
            reason: "blocked by path-guard: `find ... -exec`/`-delete` can affect an unbounded set of matched files, not just the ones visible in the command text. Explain to the user exactly what this will match and delete/run, and ask for explicit confirmation before retrying. Add a matching substring to .pirs/path-guard-allow.txt if this specific find command is expected."
        };
    }

    let tokens = cmd.split(" ");
    if tokens.len() == 0 {
        return ();
    }
    let verb = tokens[0];
    if !SENSITIVE_VERBS.contains(verb) {
        return ();
    }
    for i in 1..tokens.len() {
        let t = tokens[i];
        if is_outside_cwd(t, cwd) {
            return #{
                block: true,
                reason: `blocked by path-guard: '${verb}' targets '${t}', which is outside the working directory (${cwd}). Explain to the user what this will do to that path and ask for explicit confirmation before retrying. If this path is expected, add it (or a matching substring of this command) to .pirs/path-guard-allow.txt.`
            };
        }
    }
    ()
}
