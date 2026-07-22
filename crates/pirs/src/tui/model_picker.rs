//! In-TUI model picker with fuzzy search (portable names + catalog pins).

use ratatui::layout::Rect;
use ratatui::style::Modifier;
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph};

use super::theme::Theme;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ModelPickerTarget {
    /// Set `--model` / agent.model
    Exec,
    /// Set plan-model
    Plan,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelHit {
    /// Value applied to the agent (`qwen-plus` or `openrouter/…`).
    pub id: String,
    /// Extra display (tier / backend label).
    pub detail: String,
    pub kind: &'static str, // "portable" | "catalog"
    pub score: i64,
}

#[derive(Debug, Clone)]
pub(crate) struct ModelPicker {
    pub target: ModelPickerTarget,
    pub query: String,
    pub sel: usize,
    /// Precomputed universe (portable + catalog); filtered each keystroke.
    pub universe: Vec<ModelHit>,
    pub hits: Vec<ModelHit>,
}

impl ModelPicker {
    pub fn open(target: ModelPickerTarget, initial_query: &str) -> Self {
        let universe = build_universe();
        let mut p = Self {
            target,
            query: initial_query.to_string(),
            sel: 0,
            universe,
            hits: Vec::new(),
        };
        p.refilter();
        p
    }

    pub fn refilter(&mut self) {
        let q = self.query.trim();
        let mut scored: Vec<ModelHit> = self
            .universe
            .iter()
            .filter_map(|h| {
                let score = fuzzy_score(q, &h.id).or_else(|| fuzzy_score(q, &h.detail))?;
                Some(ModelHit {
                    id: h.id.clone(),
                    detail: h.detail.clone(),
                    kind: h.kind,
                    score,
                })
            })
            .collect();
        scored.sort_by(|a, b| {
            b.score
                .cmp(&a.score)
                .then_with(|| a.id.len().cmp(&b.id.len()))
                .then_with(|| a.id.cmp(&b.id))
        });
        // Cap for UI + typing latency.
        scored.truncate(80);
        self.hits = scored;
        if self.sel >= self.hits.len() {
            self.sel = self.hits.len().saturating_sub(1);
        }
    }

    pub fn selected(&self) -> Option<&ModelHit> {
        self.hits.get(self.sel)
    }

    pub fn title(&self) -> &'static str {
        match self.target {
            ModelPickerTarget::Exec => " model · fuzzy · ↑↓ enter · esc ",
            ModelPickerTarget::Plan => " plan-model · fuzzy · ↑↓ enter · esc ",
        }
    }
}

/// Build candidate list: portable aliases + cached catalog pins.
pub(crate) fn build_universe() -> Vec<ModelHit> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    // Load registry the same way CLI does (builtins + user + project).
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::registry::load_secrets_env();
    let reg = crate::registry::load_registry_layers(&cwd);

    for m in &reg.models {
        if !seen.insert(m.alias.clone()) {
            continue;
        }
        let backends: Vec<&str> = m.serve.iter().map(|s| s.backend.as_str()).collect();
        let tier = m.tier.as_deref().unwrap_or("portable");
        out.push(ModelHit {
            id: m.alias.clone(),
            detail: format!("{tier} · {}", backends.join(",")),
            kind: "portable",
            score: 0,
        });
    }

    // Cached catalogs → pin strings (backend/id).
    for b in &reg.backends {
        if let Some(cat) = pirs_ai::load_catalog(&b.name) {
            for m in cat.models {
                let id = pirs_ai::format_pin(&b.name, &m.id);
                if !seen.insert(id.clone()) {
                    continue;
                }
                let detail = m
                    .name
                    .clone()
                    .unwrap_or_else(|| format!("catalog · {}", b.name));
                out.push(ModelHit {
                    id,
                    detail,
                    kind: "catalog",
                    score: 0,
                });
            }
        }
    }

    // Always offer a few pin examples even with empty catalogs.
    for (id, detail) in [
        ("dashscope/qwen3.5-plus", "pin example"),
        ("openrouter/deepseek/deepseek-v4-flash", "pin example"),
        ("openai/gpt-4o-mini", "pin example"),
    ] {
        if seen.insert(id.into()) {
            out.push(ModelHit {
                id: id.into(),
                detail: detail.into(),
                kind: "catalog",
                score: 0,
            });
        }
    }

    out
}

/// Simple fuzzy score: substring boost + subsequence match. Higher is better.
/// `None` = no match (unless query empty).
pub(crate) fn fuzzy_score(query: &str, candidate: &str) -> Option<i64> {
    let q = query.trim().to_ascii_lowercase();
    if q.is_empty() {
        return Some(0);
    }
    let c = candidate.to_ascii_lowercase();

    // Exact
    if c == q {
        return Some(10_000);
    }
    // Prefix
    if c.starts_with(&q) {
        return Some(5_000 - c.len() as i64);
    }
    // Contiguous substring
    if let Some(pos) = c.find(&q) {
        return Some(3_000 - pos as i64 * 10 - c.len() as i64);
    }
    // All query tokens as substrings (space/slash split)
    let tokens: Vec<&str> = q
        .split(|ch: char| ch == ' ' || ch == '/' || ch == '-' || ch == '_')
        .filter(|t| !t.is_empty())
        .collect();
    if tokens.len() > 1 && tokens.iter().all(|t| c.contains(t)) {
        let mut score = 2_000i64;
        for t in &tokens {
            if let Some(p) = c.find(t) {
                score -= p as i64;
            }
        }
        return Some(score);
    }
    // Subsequence (fuzzy): q chars appear in order in c
    let mut ci = c.chars().peekable();
    let mut gaps = 0i64;
    let mut matched = 0i64;
    for qc in q.chars() {
        let mut found = false;
        let mut skip = 0i64;
        while let Some(&cc) = ci.peek() {
            ci.next();
            if cc == qc {
                found = true;
                matched += 1;
                gaps += skip;
                break;
            }
            skip += 1;
        }
        if !found {
            return None;
        }
    }
    Some(1_000 + matched * 20 - gaps * 3 - c.len() as i64)
}

pub(crate) fn draw_model_picker(
    frame: &mut ratatui::Frame,
    area: Rect,
    picker: &ModelPicker,
    theme: &Theme,
) {
    let w = area.width.clamp(48, 72);
    let h = area.height.clamp(12, 22);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 3;
    let rect = Rect {
        x,
        y,
        width: w,
        height: h,
    };

    frame.render_widget(Clear, rect);
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(theme.border_focus)
        .title(Span::styled(picker.title(), theme.brand));
    let inner = block.inner(rect);
    frame.render_widget(block, rect);

    // query line + results
    let chunks = ratatui::layout::Layout::default()
        .direction(ratatui::layout::Direction::Vertical)
        .constraints([
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Length(1),
            ratatui::layout::Constraint::Min(3),
            ratatui::layout::Constraint::Length(1),
        ])
        .split(inner);

    let q_line = Line::from(vec![
        Span::styled(" ❯ ", theme.accent),
        Span::styled(picker.query.clone(), theme.input),
        Span::styled("▌", theme.accent),
    ]);
    frame.render_widget(Paragraph::new(q_line), chunks[0]);

    let meta = if picker.hits.is_empty() {
        format!(
            "  0 hits · {} candidates · /models refresh outside if catalogs empty",
            picker.universe.len()
        )
    } else {
        format!(
            "  {} hits · {} in index · enter apply · esc close",
            picker.hits.len(),
            picker.universe.len()
        )
    };
    frame.render_widget(
        Paragraph::new(Span::styled(meta, theme.dim)),
        chunks[1],
    );

    let max_rows = chunks[2].height as usize;
    let sel = picker.sel.min(picker.hits.len().saturating_sub(1));
    let start = if sel >= max_rows {
        sel + 1 - max_rows
    } else {
        0
    };
    let mut lines = Vec::new();
    if picker.hits.is_empty() {
        lines.push(Line::from(Span::styled(
            "  no match — try `deepseek`, `qwen`, `claude`, or pin `dashscope/…`",
            theme.placeholder,
        )));
    } else {
        for (i, hit) in picker.hits.iter().enumerate().skip(start).take(max_rows) {
            let selected = i == sel;
            let style = if selected {
                theme.brand.add_modifier(Modifier::REVERSED)
            } else if hit.kind == "portable" {
                theme.plan
            } else {
                theme.assistant_text
            };
            let kind_style = if selected {
                theme.brand.add_modifier(Modifier::REVERSED)
            } else {
                theme.dim
            };
            let mark = if selected { "›" } else { " " };
            let kind_tag = if hit.kind == "portable" {
                "any"
            } else {
                "pin"
            };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), style),
                Span::styled(format!("{kind_tag:<4} "), kind_style),
                Span::styled(truncate(&hit.id, (w as usize).saturating_sub(24)), style),
                Span::styled(
                    format!("  {}", truncate(&hit.detail, 20)),
                    kind_style,
                ),
            ]));
        }
    }
    frame.render_widget(Paragraph::new(lines), chunks[2]);

    frame.render_widget(
        Paragraph::new(Span::styled(
            "  portable = bare name · pin = backend/id · catalogs need: pirs models refresh",
            theme.placeholder,
        )),
        chunks[3],
    );
}

fn truncate(s: &str, max: usize) -> String {
    if max == 0 {
        return String::new();
    }
    if s.chars().count() <= max {
        return s.to_string();
    }
    format!(
        "{}…",
        s.chars().take(max.saturating_sub(1)).collect::<String>()
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fuzzy_prefers_prefix() {
        let a = fuzzy_score("qwen", "qwen-plus").unwrap();
        let b = fuzzy_score("qwen", "openrouter/qwen/qwen3.5-plus").unwrap();
        assert!(a > b, "prefix portable should rank above long pin: {a} vs {b}");
    }

    #[test]
    fn fuzzy_subsequence_matches() {
        assert!(fuzzy_score("dsf", "deepseek-v4-flash").is_some());
        assert!(fuzzy_score("xyzzy", "deepseek").is_none());
    }

    #[test]
    fn fuzzy_tokens() {
        assert!(fuzzy_score("deep flash", "deepseek-v4-flash").is_some());
    }
}
