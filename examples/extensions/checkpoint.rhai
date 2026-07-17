// checkpoint.rhai — periodic session snapshots + /checkpoints + /restore.

const SNAPSHOT_EVERY = 5;

fn ckpt_dir() {
    let cwd = exec("pwd").output;
    cwd.trim();
    `${cwd}/.pirs/checkpoints`
}

fn on_context(messages) {
    let n = (state_get("ctx_calls") ?? 0) + 1;
    state_set("ctx_calls", n);
    if n % SNAPSHOT_EVERY != 0 {
        return messages;
    }
    let dir = ckpt_dir();
    exec(`mkdir -p ${dir}`, 5);
    let entry = #{
        ts: now_millis(),
        n: n,
        messages: messages
    };
    fs_append(`${dir}/log.jsonl`, entry.to_json() + "\n");
    messages
}

register_command("checkpoints", "List session checkpoints");

fn cmd_checkpoints(args) {
    let content = fs_read(`${ckpt_dir()}/log.jsonl`);
    if content == "" {
        return "no checkpoints yet";
    }
    let lines = content.split("\n").filter(|l| l != "");
    let out = "";
    let start = if lines.len() > 10 { lines.len() - 10 } else { 0 };
    for i in start..lines.len() {
        let e = lines[i].parse_json();
        out = out + `#${i} ts=${e.ts} messages=${e.messages.len()}\n`;
    }
    out
}

register_command("restore", "Restore context note from checkpoint N");

fn cmd_restore(args) {
    let content = fs_read(`${ckpt_dir()}/log.jsonl`);
    if content == "" {
        return "no checkpoints";
    }
    let lines = content.split("\n").filter(|l| l != "");
    let raw = args;
    raw.trim();
    if raw == "" {
        return `usage: /restore <0..${lines.len() - 1}>`;
    }
    let idx = raw.parse_json();
    if idx < 0 || idx >= lines.len() {
        return `usage: /restore <0..${lines.len() - 1}>`;
    }
    let e = lines[idx].parse_json();
    let summary = `Checkpoint #${idx} had ${e.messages.len()} messages. Last message: ` +
        e.messages[e.messages.len() - 1].content.to_string().sub_string(0, 300);
    state_set("restore_pin", summary);
    `${summary}\n(will be pinned into the next prompt's context)`
}

fn on_steering() {
    if state_has("restore_pin") {
        let pin = state_get("restore_pin");
        state_del("restore_pin");
        return `[restored checkpoint context]\n${pin}`;
    }
    ()
}
