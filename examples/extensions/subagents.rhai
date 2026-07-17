// subagents.rhai — named sub-agents from .pirs/agents/*.md (and ~/.pirs/agents).
// File format: frontmatter (name, description, model?) + persona prompt body.
// Each file becomes a tool: <name>(task) running the persona in a fresh context.

fn agent_dirs() {
    let cwd = exec("pwd").output;
    cwd.trim();
    let home = exec("printf %s \"$HOME\"").output;
    home.trim();
    [`${cwd}/.pirs/agents`, `${home}/.pirs/agents`]
}

fn cleaned(s) {
    let t = s;
    t.trim();
    t
}

fn parse_agent_file(path) {
    let content = fs_read(path);
    if content == "" {
        return ();
    }
    if !content.starts_with("---") {
        return ();
    }
    let rest = content.sub_string(3, content.len() - 3);
    let end = rest.index_of("\n---");
    if end < 0 {
        return ();
    }
    let head = rest.sub_string(0, end);
    let body = rest.sub_string(end + 4, rest.len() - end - 4);
    let agent = #{ name: "", description: "", model: "", prompt: body };
    for line in head.split("\n") {
        if line.contains("name:") {
            agent.name = cleaned(line.sub_string(5, line.len() - 5));
        }
        if line.contains("description:") {
            agent.description = cleaned(line.sub_string(12, line.len() - 12));
        }
        if line.contains("model:") {
            agent.model = cleaned(line.sub_string(6, line.len() - 6));
        }
    }
    agent
}

let agent_count = 0;
for dir in agent_dirs() {
    let listing = exec(`ls ${dir} 2>/dev/null`, 5).output;
    for file in listing.split("\n") {
        if !file.ends_with(".md") {
            continue;
        }
        let agent = parse_agent_file(`${dir}/${file}`);
        if agent == () || agent.name == "" {
            continue;
        }
        let desc = if agent.description == "" { `Sub-agent: ${agent.name}` } else { agent.description };
        register_tool(agent.name, desc, #{
            type: "object",
            properties: #{ task: #{ type: "string" } },
            required: ["task"]
        });
        state_set(`persona_${agent.name}`, #{ prompt: agent.prompt, model: agent.model });
        agent_count += 1;
    }
}
state_set("agent_count", agent_count);

fn tool_dispatch(name, args) {
    let persona = state_get(`persona_${name}`) ?? ();
    if persona == () {
        return `unknown sub-agent: ${name}`;
    }
    let task = `You are the '${name}' specialist. Follow your persona exactly.\n\nPERSONA:\n${persona.prompt}\n\nTASK:\n${args.task}`;
    if persona.model == "" {
        run_subagent(task)
    } else {
        run_subagent(task, persona.model)
    }
}
