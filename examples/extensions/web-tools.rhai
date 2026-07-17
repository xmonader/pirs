// web-tools.rhai — web_fetch and web_search via curl.

register_tool("web_fetch", "Fetch a URL and return its text content (truncated).", #{
    type: "object",
    properties: #{
        url: #{ type: "string" },
        max_chars: #{ type: "integer" }
    },
    required: ["url"]
});

fn tool_web_fetch(args) {
    let r = exec(`curl -sL --max-time 25 -A "pirs-agent" "${args.url}"`, 30);
    if r.code != 0 {
        return `fetch failed (exit ${r.code}): ${r.output.sub_string(0, 300)}`;
    }
    let max = args.max_chars ?? 12000;
    let out = r.output;
    if out.len() > max {
        out = out.sub_string(0, max) + `\n[truncated at ${max} chars]`;
    }
    if out.trim() == "" {
        return "(empty response)";
    }
    out
}

register_tool("web_search", "Search the web (DuckDuckGo lite) and return result snippets.", #{
    type: "object",
    properties: #{ query: #{ type: "string" } },
    required: ["query"]
});

fn tool_web_search(args) {
    let q = args.query;
    q.replace(" ", "+");
    let r = exec(`curl -sL --max-time 20 -A "Mozilla/5.0" "https://lite.duckduckgo.com/lite/?q=${q}"`, 25);
    if r.code != 0 {
        return `search failed (exit ${r.code})`;
    }
    let out = r.output;
    if out.len() > 6000 {
        out = out.sub_string(0, 6000) + "\n[truncated]";
    }
    out
}
