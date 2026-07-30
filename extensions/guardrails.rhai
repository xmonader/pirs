// guardrails.rhai — block destructive commands; the model must ask the user.
// Install: cp to .pirs/extensions/ or ~/.pirs/extensions/
//
// Hard catastrophic roots (`rm -rf /`, curl|sh, mkfs) are also enforced in
// Rust (`pirs_tools::shell_safety`) so disabling this pack does not remove them.
// This pack adds force-push and similar policy denials.

const BLOCKED = [
    "rm -rf /",
    "rm -rf ~",
    "rm -rf .",
    "rm -fr /",
    "rm -fr ~",
    "dd if=",
    "mkfs",
    ":(){:|:&};:",
    "chmod -R 777 /",
    "git push --force",
    "git push -f ",
    "curl ",
    "wget ",
];

fn flatten(cmd) {
    // Newlines are command separators in bash — normalize before matching.
    let s = `${cmd}`;
    s.replace("\n", " ");
    s.replace("\r", " ");
    s
}

fn on_tool_call(id, name, args) {
    if name != "bash" {
        return ();
    }
    let cmd = flatten(`${args.command}`);
    for pattern in BLOCKED {
        if cmd.contains(pattern) {
            if (pattern == "curl " || pattern == "wget ")
                && !cmd.contains("| sh")
                && !cmd.contains("|sh")
                && !cmd.contains("| bash")
                && !cmd.contains("|bash")
                && !cmd.contains("|/bin/sh")
                && !cmd.contains("|/bin/bash")
            {
                continue;
            }
            // Don't block `rm -rf /tmp/...` via the root pattern (Rust also scopes this).
            if pattern == "rm -rf /" || pattern == "rm -fr /" {
                let idx = cmd.index_of(pattern);
                if idx >= 0 {
                    let after = cmd.sub_string(idx + pattern.len());
                    if after.len() > 0 {
                        let c0 = after.sub_string(0, 1);
                        if c0 != " " && c0 != ";" && c0 != "&" && c0 != "|"
                            && c0 != "\"" && c0 != "'" && !after.starts_with("--")
                        {
                            continue;
                        }
                    }
                }
            }
            return #{
                block: true,
                reason: `blocked by guardrails: '${pattern}' is destructive. Explain to the user what you intended and ask for explicit confirmation before retrying.`
            };
        }
    }
    ()
}
