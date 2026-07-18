// swarm.rhai — work-queue discipline over the hive: queen posts packets,
// workers claim, results merge. Multi-instance coordination.

fn hive() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/swarm.jsonl`
}

fn queue_read() {
    let content = fs_read(hive());
    if content == "" {
        return [];
    }
    content.split("\n").filter(|l| l != "").map(|l| l.parse_json())
}

fn queue_write(entries) {
    let out = "";
    for e in entries {
        out = out + e.to_json() + "\n";
    }
    fs_write(hive(), out);
}

register_tool("swarm_post", "Post a work packet for the swarm (queen/planner use).", #{
    type: "object",
    properties: #{
        task: #{ type: "string" },
        role: #{ type: "string" }
    },
    required: ["task"]
});

fn tool_swarm_post(args) {
    let entries = queue_read();
    let id = (state_get("packet_seq") ?? 0) + 1;
    state_set("packet_seq", id);
    entries.push(#{
        id: id,
        task: args.task,
        role: args.role ?? "worker",
        status: "open",
        result: (),
        ts: now_millis()
    });
    queue_write(entries);
    `packet #${id} posted`
}

register_tool("swarm_claim", "Claim the oldest open work packet (worker use). Returns the packet or empty.", #{
    type: "object",
    properties: #{}
});

fn tool_swarm_claim(args) {
    let entries = queue_read();
    for i in 0..entries.len() {
        if entries[i].status == "open" {
            entries[i]["status"] = "claimed";
            queue_write(entries);
            let e = entries[i];
            return `packet #${e.id}: ${e.task}`;
        }
    }
    "no open packets"
}

register_tool("swarm_done", "Mark a claimed packet done with its result.", #{
    type: "object",
    properties: #{
        id: #{ type: "integer" },
        result: #{ type: "string" }
    },
    required: ["id", "result"]
});

fn tool_swarm_done(args) {
    let entries = queue_read();
    for i in 0..entries.len() {
        if entries[i].id == args.id {
            entries[i]["status"] = "done";
            entries[i]["result"] = args.result;
            queue_write(entries);
            return `packet #${args.id} done`;
        }
    }
    `no such packet: ${args.id}`
}

register_tool("swarm_status", "List packets and their status.", #{
    type: "object",
    properties: #{}
});

fn tool_swarm_status(args) {
    let entries = queue_read();
    if entries.len() == 0 {
        return "swarm queue empty";
    }
    let out = "";
    for e in entries {
        let res = if e.result == () { "" } else { " -> " + e.result.sub_string(0, 60) };
        out = out + `#${e.id} [${e.status}] ${e.task.sub_string(0, 60)}${res}\n`;
    }
    out
}
