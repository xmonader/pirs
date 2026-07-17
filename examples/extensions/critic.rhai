// critic.rhai — interleaved mid-run critic. Every N edits, a background
// sub-agent reviews the diff and steers corrections before the run drifts.

const CRITIC_PROMPT = "You are a ruthless senior reviewer examining an in-progress diff. List only REAL problems: bugs, wrong abstraction, broken invariants, missing error handling. Be concrete with file/line references. If the direction is sound, reply with exactly: SOUND";
const CRITIC_EVERY = 3;

fn on_tool_result(id, name, result) {
    if name != "edit" && name != "write" {
        return ();
    }
    if result.isError {
        return ();
    }
    let n = (state_get("edit_count") ?? 0) + 1;
    state_set("edit_count", n);
    if n % CRITIC_EVERY != 0 || state_has("critic_pending") {
        return ();
    }
    let diff = exec("git diff HEAD 2>/dev/null | head -c 8000", 10).output;
    let d = diff;
    d.trim();
    if d == "" {
        return ();
    }
    state_set("critic_pending", true);
    spawn_subagent(`${CRITIC_PROMPT}\n\n${d}`, "", "critic");
    ()
}

fn on_steering() {
    let items = inbox();
    for item in items {
        if item.tag == "critic" {
            state_del("critic_pending");
            if !item.result.contains("SOUND") {
                return `Mid-run critic review:\n${item.result}\n\nAddress the real points, ignore the rest, then continue.`;
            }
        }
    }
    ()
}
