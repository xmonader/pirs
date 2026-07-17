// skill-crystallizer.rhai — after a successful run, distill what worked into
// a reusable SKILL.md so future sessions start smarter.

const DISTILL_PROMPT = `Distill this completed session into a reusable skill.
Output ONLY the skill content as markdown with this exact frontmatter:
---
name: <short-kebab-name>
description: <one sentence: when should an agent use this skill>
---
<concise steps/gotchas worth reusing, max 30 lines>
If nothing generally reusable was learned (trivial task), output exactly: NOTHING`;

fn skills_dir() {
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    `${home}/.pirs/skills`
}

fn on_event(ty, data) {
    if ty != "agent_end" {
        return ();
    }
    if state_has("distilled") || (state_get("edit_count") ?? 0) < 2 {
        return ();
    }
    state_set("distilled", true);
    let transcript = exec("tail -c 6000 ~/.pirs/audit.jsonl 2>/dev/null || true", 10).output;
    let t = transcript;
    t.trim();
    if t == "" {
        return ();
    }
    let skill = run_subagent(`${DISTILL_PROMPT}\n\nSESSION LOG:\n${transcript}`);
    let s = skill;
    s.trim();
    if s == "" || s.starts_with("NOTHING") || !s.starts_with("---") {
        return ();
    }
    let name_line = s.split("\n").filter(|l| l.starts_with("name:"));
    if name_line.len() == 0 {
        return ();
    }
    let name = name_line[0].sub_string(5, name_line[0].len() - 5);
    name.trim();
    let dir = `${skills_dir()}/${name}`;
    exec(`mkdir -p ${dir}`, 5);
    fs_append(`${dir}/SKILL.md`, skill + "\n");
    state_set("crystallized", name);
}

fn on_tool_result(id, name, result) {
    if (name == "edit" || name == "write") && !result.isError {
        state_set("edit_count", (state_get("edit_count") ?? 0) + 1);
    }
    ()
}

fn on_steering() {
    if state_has("crystallized") {
        let name = state_get("crystallized");
        state_del("crystallized");
        return `A new skill '${name}' was crystallized from this session into ~/.pirs/skills/. It will be available in future sessions.`;
    }
    ()
}
