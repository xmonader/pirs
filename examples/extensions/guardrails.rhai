// guardrails.rhai — block destructive commands; the model must ask the user.
// Install: cp to .pirs/extensions/ or ~/.pirs/extensions/

const BLOCKED = [
    "rm -rf /",
    "rm -rf ~",
    "rm -rf .",
    "dd if=",
    "mkfs",
    ":(){:|:&};:",
    "chmod -R 777 /",
    "git push --force",
    "git push -f ",
    "curl ",
    "wget ",
];

fn on_tool_call(id, name, args) {
    if name != "bash" {
        return ();
    }
    let cmd = args.command;
    for pattern in BLOCKED {
        if cmd.contains(pattern) {
            if (pattern == "curl " || pattern == "wget ") && !cmd.contains("| sh") && !cmd.contains("|sh") && !cmd.contains("| bash") && !cmd.contains("|bash") {
                continue;
            }
            return #{
                block: true,
                reason: `blocked by guardrails: '${pattern}' is destructive. Explain to the user what you intended and ask for explicit confirmation before retrying.`
            };
        }
    }
    ()
}
