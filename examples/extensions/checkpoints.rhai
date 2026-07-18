// checkpoints.rhai — VCS-free per-file checkpoints (qwen /restore): every
// mutating tool call snapshots the target file first. /restore <path> rewinds.

fn ckpt_root() {
    let cwd = exec("pwd").output;
    cwd.trim();
    `${cwd}/.pirs/checkpoints`
}

fn on_tool_call(id, name, args) {
    if name != "edit" && name != "write" {
        return ();
    }
    let path = args.path ?? "";
    if path == "" {
        return ();
    }
    let exists = exec(`test -f "${path}" && echo yes || echo no`, 5).output;
    exists.trim();
    if exists != "yes" {
        return ();
    }
    let stamp = now_millis();
    let safe_name = path;
    safe_name.replace("/", "_");
    let dest = `${ckpt_root()}/${safe_name}`;
    exec(`mkdir -p ${ckpt_root()}`, 5);
    exec(`cp "${path}" "${dest}"`, 10);
    let log = #{ path: path, dest: dest, ts: stamp };
    fs_append(`${ckpt_root()}/log.jsonl`, log.to_json() + "\n");
    ()
}

register_command("checkpoints", "List file checkpoints");

fn cmd_checkpoints(args) {
    let content = fs_read(`${ckpt_root()}/log.jsonl`);
    if content == "" {
        return "no checkpoints";
    }
    let lines = content.split("\n").filter(|l| l != "");
    let out = "";
    let start = if lines.len() > 10 { lines.len() - 10 } else { 0 };
    for i in start..lines.len() {
        let e = lines[i].parse_json();
        out = out + `${e.ts}  ${e.path}\n`;
    }
    out
}

register_command("restore", "Restore a file from its latest checkpoint: /restore <path>");

fn cmd_restore(args) {
    let path = args;
    path.trim();
    if path == "" {
        return "usage: /restore <path>";
    }
    let safe_name = path;
    safe_name.replace("/", "_");
    let dest = `${ckpt_root()}/${safe_name}`;
    let exists = exec(`test -f "${dest}" && echo yes || echo no`, 5).output;
    exists.trim();
    if exists != "yes" {
        return `no checkpoint for ${path}`;
    }
    let r = exec(`cp "${dest}" "${path}"`, 10);
    if r.code == 0 {
        `restored ${path} from checkpoint`
    } else {
        `restore failed: ${r.output}`
    }
}
