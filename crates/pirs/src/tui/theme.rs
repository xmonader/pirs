use ratatui::style::{Color, Modifier, Style};

// ── Theme (semantic roles; keep slate/cyan/violet DNA) ──────────────────────

pub(crate) struct Theme {
    pub(crate) brand: Style,
    pub(crate) header_bg: Style,
    pub(crate) user_label: Style,
    pub(crate) user_text: Style,
    pub(crate) assistant_label: Style,
    pub(crate) assistant_text: Style,
    pub(crate) thinking: Style,
    pub(crate) tool_name: Style,
    pub(crate) tool_args: Style,
    pub(crate) tool_ok: Style,
    pub(crate) tool_err: Style,
    pub(crate) path: Style,
    pub(crate) command: Style,
    pub(crate) success: Style,
    pub(crate) warning: Style,
    pub(crate) system: Style,
    pub(crate) error: Style,
    pub(crate) dim: Style,
    pub(crate) accent: Style,
    pub(crate) border: Style,
    pub(crate) border_focus: Style,
    pub(crate) input: Style,
    pub(crate) input_border: Style,
    pub(crate) approval: Style,
    pub(crate) plan: Style,
    pub(crate) yolo: Style,
    pub(crate) status: Style,
    pub(crate) code: Style,
    pub(crate) code_block: Style,
    pub(crate) bold: Style,
    pub(crate) heading: Style,
    pub(crate) placeholder: Style,
}

impl Theme {
    pub(crate) fn default_dark() -> Self {
        if std::env::var("PIRS_TUI_THEME")
            .map(|v| v.eq_ignore_ascii_case("mono"))
            .unwrap_or(false)
        {
            return Self::mono();
        }
        Self {
            brand: Style::default()
                .fg(Color::Rgb(125, 211, 252))
                .add_modifier(Modifier::BOLD),
            header_bg: Style::default().fg(Color::Rgb(148, 163, 184)),
            user_label: Style::default()
                .fg(Color::Rgb(52, 211, 153))
                .add_modifier(Modifier::BOLD),
            user_text: Style::default().fg(Color::Rgb(209, 250, 229)),
            assistant_label: Style::default()
                .fg(Color::Rgb(167, 139, 250))
                .add_modifier(Modifier::BOLD),
            assistant_text: Style::default().fg(Color::Rgb(226, 232, 240)),
            thinking: Style::default()
                .fg(Color::Rgb(100, 116, 139))
                .add_modifier(Modifier::ITALIC),
            tool_name: Style::default()
                .fg(Color::Rgb(251, 191, 36))
                .add_modifier(Modifier::BOLD),
            tool_args: Style::default().fg(Color::Rgb(148, 163, 184)),
            tool_ok: Style::default().fg(Color::Rgb(100, 116, 139)),
            tool_err: Style::default().fg(Color::Rgb(248, 113, 113)),
            path: Style::default().fg(Color::Rgb(251, 146, 60)),
            command: Style::default().fg(Color::Rgb(250, 204, 21)),
            success: Style::default().fg(Color::Rgb(74, 222, 128)),
            warning: Style::default().fg(Color::Rgb(251, 191, 36)),
            system: Style::default().fg(Color::Rgb(100, 116, 139)),
            error: Style::default()
                .fg(Color::Rgb(248, 113, 113))
                .add_modifier(Modifier::BOLD),
            dim: Style::default().fg(Color::Rgb(71, 85, 105)),
            accent: Style::default().fg(Color::Rgb(56, 189, 248)),
            border: Style::default().fg(Color::Rgb(51, 65, 85)),
            border_focus: Style::default().fg(Color::Rgb(56, 189, 248)),
            input: Style::default().fg(Color::Rgb(241, 245, 249)),
            input_border: Style::default().fg(Color::Rgb(56, 189, 248)),
            approval: Style::default()
                .fg(Color::Rgb(251, 113, 133))
                .add_modifier(Modifier::BOLD),
            plan: Style::default().fg(Color::Rgb(74, 222, 128)),
            yolo: Style::default()
                .fg(Color::Rgb(248, 113, 113))
                .add_modifier(Modifier::BOLD),
            status: Style::default().fg(Color::Rgb(148, 163, 184)),
            code: Style::default().fg(Color::Rgb(125, 211, 252)),
            code_block: Style::default().fg(Color::Rgb(186, 230, 253)),
            bold: Style::default()
                .fg(Color::Rgb(248, 250, 252))
                .add_modifier(Modifier::BOLD),
            heading: Style::default()
                .fg(Color::Rgb(165, 243, 252))
                .add_modifier(Modifier::BOLD),
            placeholder: Style::default().fg(Color::Rgb(71, 85, 105)),
        }
    }

    fn mono() -> Self {
        let base = Style::default();
        let bold = Style::default().add_modifier(Modifier::BOLD);
        let dim = Style::default().add_modifier(Modifier::DIM);
        let italic = Style::default().add_modifier(Modifier::ITALIC);
        Self {
            brand: bold,
            header_bg: dim,
            user_label: bold,
            user_text: base,
            assistant_label: bold,
            assistant_text: base,
            thinking: italic,
            tool_name: bold,
            tool_args: dim,
            tool_ok: dim,
            tool_err: bold,
            path: base,
            command: base,
            success: base,
            warning: bold,
            system: dim,
            error: bold,
            dim,
            accent: bold,
            border: dim,
            border_focus: bold,
            input: base,
            input_border: bold,
            approval: bold,
            plan: base,
            yolo: bold,
            status: dim,
            code: base,
            code_block: base,
            bold,
            heading: bold,
            placeholder: dim,
        }
    }
}


/// Composer border color by approval mode + session state (qwen/vibe pattern).
pub(crate) fn composer_mode_style(
    theme: &Theme,
    approval_mode: &str,
    running: bool,
    pending_approval: bool,
) -> Style {
    if pending_approval {
        return theme.approval;
    }
    if running {
        return theme.warning;
    }
    let m = approval_mode.to_ascii_lowercase();
    if m.contains("yolo") || m == "auto" || m.contains("auto-approve") {
        return theme.yolo;
    }
    if m.contains("plan") {
        return theme.plan;
    }
    if m.contains("ask") {
        return theme.warning;
    }
    theme.input_border
}

pub(crate) fn format_elapsed(secs: u64) -> String {
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m{:02}s", secs / 60, secs % 60)
    }
}

pub(crate) fn approval_grace_elapsed(opened: Option<std::time::Instant>) -> bool {
    match opened {
        None => true,
        Some(t) => t.elapsed() >= std::time::Duration::from_millis(400),
    }
}
