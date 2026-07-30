// ── Slash command catalog + completion ──────────────────────────────────────

#[derive(Clone, Debug)]
pub(crate) struct SlashCmd {
    pub(crate) name: String,
    pub(crate) desc: String,
}

/// Built-in TUI slash commands (not extension-provided).
const BUILTIN: &[(&str, &str)] = &[
    ("/help", "show help overlay"),
    ("/tour", "first-run journey / starters"),
    ("/model", "fuzzy picker · or set backend/id"),
    ("/models", "fuzzy search models · refresh catalogs"),
    ("/backends", "list backends and key status"),
    ("/backend", "add name url env  — new subscription"),
    ("/key", "set SECRET=value in secrets.env"),
    ("/setup", "keys + backend setup status"),
    ("/thoughts", "expand/collapse model thinking"),
    ("/context", "show multi-root work context"),
    ("/plan-model", "planner fuzzy picker or set"),
    (
        "/strategy",
        "plan-exec | plan-critic-exec | weak-drive | monolithic | none",
    ),
    ("/stats", "session usage + timing"),
    ("/usage", "alias for /stats"),
    ("/status", "runtime features, autonomy, packs, caps"),
    ("/features", "alias for /status"),
    ("/copy", "copy last assistant reply to clipboard"),
    ("/yank", "alias for /copy"),
    ("/autonomy", "plan | edit | full  (one tool-access knob)"),
    ("/plan", "autonomy plan — read-only"),
    ("/edit", "autonomy edit — writes, no shell"),
    ("/act", "autonomy full — all tools (alias /yolo)"),
    ("/yolo", "autonomy full — all tools, no prompts"),
    ("/full", "alias for /yolo"),
    ("/permission", "legacy alias for /autonomy"),
    ("/profile", "legacy agent-profile (prefer /autonomy)"),
    ("/undo", "rewind last user turn"),
    ("/compact", "compact conversation context"),
    ("/doctor", "environment health check"),
    ("/audit", "tail action audit log"),
    ("/image", "attach image path to context"),
    ("/checkpoint", "list | create | restore [id]"),
    ("/clear", "clear chat screen"),
    ("/quit", "exit TUI"),
];

/// Build the full slash catalog: builtins first, then extension commands
/// (e.g. `/goal`, `/btw`) that are not already covered by a builtin name.
pub(crate) fn slash_catalog(ext_cmds: &[(String, String)]) -> Vec<SlashCmd> {
    let mut out: Vec<SlashCmd> = BUILTIN
        .iter()
        .map(|(n, d)| SlashCmd {
            name: (*n).to_string(),
            desc: (*d).to_string(),
        })
        .collect();
    let builtin_names: std::collections::HashSet<&str> = BUILTIN.iter().map(|(n, _)| *n).collect();
    for (name, desc) in ext_cmds {
        let slash = if name.starts_with('/') {
            name.clone()
        } else {
            format!("/{name}")
        };
        if builtin_names.contains(slash.as_str()) {
            // Core TUI command wins (e.g. /undo conversation rewind).
            continue;
        }
        if out.iter().any(|c| c.name == slash) {
            continue;
        }
        out.push(SlashCmd {
            name: slash,
            desc: desc.clone(),
        });
    }
    out
}

pub(crate) fn slash_filter(prefix: &str, ext_cmds: &[(String, String)]) -> Vec<SlashCmd> {
    let all = slash_catalog(ext_cmds);
    let p = prefix.to_ascii_lowercase();
    if p.is_empty() || p == "/" {
        return all;
    }
    all.into_iter()
        .filter(|c| c.name.to_ascii_lowercase().starts_with(&p))
        .collect()
}

/// True when the input is a slash command being typed (no args yet).
pub(crate) fn slash_completing(input: &str) -> bool {
    let t = input.trim_end();
    t.starts_with('/') && !t.contains(char::is_whitespace)
}
