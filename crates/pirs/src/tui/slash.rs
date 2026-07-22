// ── Slash command catalog + completion ──────────────────────────────────────

#[derive(Clone, Copy, Debug)]
pub(crate) struct SlashCmd {
    pub(crate) name: &'static str,
    pub(crate) desc: &'static str,
}

pub(crate) const SLASH_CMDS: &[SlashCmd] = &[
    SlashCmd {
        name: "/help",
        desc: "show help overlay",
    },
    SlashCmd {
        name: "/tour",
        desc: "first-run journey / starters",
    },
    SlashCmd {
        name: "/model",
        desc: "show or set exec model",
    },
    SlashCmd {
        name: "/plan-model",
        desc: "strong planner model (none to clear)",
    },
    SlashCmd {
        name: "/strategy",
        desc: "plan-exec | plan-critic-exec | monolithic | none",
    },
    SlashCmd {
        name: "/stats",
        desc: "session usage + timing",
    },
    SlashCmd {
        name: "/usage",
        desc: "alias for /stats",
    },
    SlashCmd {
        name: "/plan",
        desc: "read-only mode (explore safely)",
    },
    SlashCmd {
        name: "/act",
        desc: "full tools (writes + shell)",
    },
    SlashCmd {
        name: "/permission",
        desc: "read-only | workspace-write | danger-full-access",
    },
    SlashCmd {
        name: "/profile",
        desc: "default | plan | accept-edits | auto-approve",
    },
    SlashCmd {
        name: "/undo",
        desc: "rewind last user turn",
    },
    SlashCmd {
        name: "/compact",
        desc: "compact conversation context",
    },
    SlashCmd {
        name: "/doctor",
        desc: "environment health check",
    },
    SlashCmd {
        name: "/audit",
        desc: "tail action audit log",
    },
    SlashCmd {
        name: "/image",
        desc: "attach image path to context",
    },
    SlashCmd {
        name: "/checkpoint",
        desc: "list | create | restore [id]",
    },
    SlashCmd {
        name: "/clear",
        desc: "clear chat screen",
    },
    SlashCmd {
        name: "/quit",
        desc: "exit TUI",
    },
];

pub(crate) fn slash_filter(prefix: &str) -> Vec<&'static SlashCmd> {
    let p = prefix.to_ascii_lowercase();
    if p.is_empty() || p == "/" {
        return SLASH_CMDS.iter().collect();
    }
    SLASH_CMDS
        .iter()
        .filter(|c| c.name.starts_with(&p))
        .collect()
}

/// True when the input is a slash command being typed (no args yet).
pub(crate) fn slash_completing(input: &str) -> bool {
    let t = input.trim_end();
    t.starts_with('/') && !t.contains(char::is_whitespace)
}

