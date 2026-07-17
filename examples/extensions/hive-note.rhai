// hive-note.rhai — shared blackboard for coordinating multiple pirs instances.

fn hive_path() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/hive.jsonl`
}

register_tool("hive_post", "Post a note to the shared hive blackboard (other pirs instances can read it).", #{
    type: "object",
    properties: #{ note: #{ type: "string" } },
    required: ["note"]
});

fn tool_hive_post(args) {
    let entry = #{ ts: now_millis(), note: args.note };
    fs_append(hive_path(), entry.to_json() + "\n");
    "Posted to hive."
}

register_tool("hive_read", "Read the latest notes from the shared hive blackboard.", #{
    type: "object",
    properties: #{ limit: #{ type: "integer" } }
});

fn tool_hive_read(args) {
    let content = fs_read(hive_path());
    if content == "" {
        return "(hive is empty)";
    }
    let lines = content.split("\n").filter(|l| l != "");
    let limit = args.limit ?? 10;
    let start = if lines.len() > limit { lines.len() - limit } else { 0 };
    let tail = lines.extract(start, lines.len() - start);
    let out = "";
    for l in tail {
        out = out + l + "\n";
    }
    out
}
