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
    /// Open picker with CLI/session registry aliases preferred in the universe.
    /// Pass `&[]` when no session aliases are available.
    pub fn open_with_aliases(
        target: ModelPickerTarget,
        initial_query: &str,
        preferred_aliases: &[String],
    ) -> Self {
        let universe = build_universe_with_aliases(preferred_aliases);
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

/// Order portable model hits: preferred aliases first **without** shadowing
/// registry tier/backend metadata. Unknown preferred IDs get a session label.
///
/// This is the pure merge used by the TUI production path when main seeds
/// `model_aliases` from the full registry (preferred ⊆ registry is the common case).
pub(crate) fn order_portable_hits(
    preferred_aliases: &[String],
    registry_models: &[(String, String)],
) -> Vec<ModelHit> {
    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();
    let reg_detail: std::collections::HashMap<&str, &str> = registry_models
        .iter()
        .map(|(alias, detail)| (alias.as_str(), detail.as_str()))
        .collect();

    for alias in preferred_aliases {
        let id = alias.trim();
        if id.is_empty() || !seen.insert(id.to_string()) {
            continue;
        }
        if let Some(detail) = reg_detail.get(id) {
            out.push(ModelHit {
                id: id.to_string(),
                detail: (*detail).to_string(),
                kind: "portable",
                score: 0,
            });
        } else {
            // Session-only pin / alias not present in the registry layer.
            out.push(ModelHit {
                id: id.to_string(),
                detail: "cli · session".into(),
                kind: "portable",
                score: 0,
            });
        }
    }

    for (alias, detail) in registry_models {
        if !seen.insert(alias.clone()) {
            continue;
        }
        out.push(ModelHit {
            id: alias.clone(),
            detail: detail.clone(),
            kind: "portable",
            score: 0,
        });
    }
    out
}

/// Build candidate list: preferred CLI aliases first, then registry + catalogs.
/// Prefer CLI-seeded aliases so `App.model_aliases` is not discarded, while
/// keeping registry tier/backend labels when the alias is known.
pub(crate) fn build_universe_with_aliases(preferred_aliases: &[String]) -> Vec<ModelHit> {
    // Load registry the same way CLI does (builtins + user + project).
    let cwd = std::env::current_dir().unwrap_or_else(|_| std::path::PathBuf::from("."));
    crate::registry::load_secrets_env();
    let reg = crate::registry::load_registry_layers(&cwd);

    let registry_models: Vec<(String, String)> = reg
        .models
        .iter()
        .map(|m| {
            let backends: Vec<&str> = m.serve.iter().map(|s| s.backend.as_str()).collect();
            let tier = m.tier.as_deref().unwrap_or("portable");
            (m.alias.clone(), format!("{tier} · {}", backends.join(",")))
        })
        .collect();

    let mut out = order_portable_hits(preferred_aliases, &registry_models);
    let mut seen: std::collections::HashSet<String> = out.iter().map(|h| h.id.clone()).collect();

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
    frame.render_widget(Paragraph::new(Span::styled(meta, theme.dim)), chunks[1]);

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
            let kind_tag = if hit.kind == "portable" { "any" } else { "pin" };
            lines.push(Line::from(vec![
                Span::styled(format!(" {mark} "), style),
                Span::styled(format!("{kind_tag:<4} "), kind_style),
                Span::styled(truncate(&hit.id, (w as usize).saturating_sub(24)), style),
                Span::styled(format!("  {}", truncate(&hit.detail, 20)), kind_style),
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
        assert!(
            a > b,
            "prefix portable should rank above long pin: {a} vs {b}"
        );
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

    /// Unknown session-only preferred aliases lead the list with a session label.
    #[test]
    fn preferred_unknown_aliases_lead_with_session_label() {
        let special = "codesweep-test-alias-xyz".to_string();
        let hits = order_portable_hits(std::slice::from_ref(&special), &[]);
        assert_eq!(hits[0].id, special);
        assert!(
            hits[0].detail.contains("session"),
            "unknown preferred should be session-labeled, not fake registry: {:?}",
            hits[0].detail
        );
        let picker = ModelPicker::open_with_aliases(
            ModelPickerTarget::Exec,
            "",
            std::slice::from_ref(&special),
        );
        assert!(
            picker.universe.iter().any(|h| h.id == special),
            "open_with_aliases must include preferred alias in universe"
        );
    }

    /// Production path: main seeds model_aliases from the full registry, so
    /// preferred ⊆ registry. Those hits must keep tier/backend detail, not
    /// a hardcoded "cli · registry" placeholder that shadows metadata.
    #[test]
    fn preferred_subset_of_registry_keeps_tier_backend_detail() {
        let registry: Vec<(String, String)> = vec![
            ("qwen-plus".into(), "strong · dashscope".into()),
            ("deepseek-v4-flash".into(), "cheap · openrouter".into()),
            ("kimi-k2.5".into(), "mid · moonshot".into()),
        ];
        // Full-registry preferred list (what main typically passes).
        let preferred: Vec<String> = registry.iter().map(|(a, _)| a.clone()).collect();
        let hits = order_portable_hits(&preferred, &registry);

        assert_eq!(hits.len(), 3, "no duplicates when preferred ⊆ registry");
        // Preferred order preserved.
        assert_eq!(hits[0].id, "qwen-plus");
        assert_eq!(hits[1].id, "deepseek-v4-flash");
        assert_eq!(hits[2].id, "kimi-k2.5");
        // Registry metadata must survive (not "cli · registry").
        assert_eq!(hits[0].detail, "strong · dashscope");
        assert_eq!(hits[1].detail, "cheap · openrouter");
        assert_eq!(hits[2].detail, "mid · moonshot");
        for h in &hits {
            assert!(
                !h.detail.contains("cli · registry") && !h.detail.eq("cli · session"),
                "registry preferred must not use placeholder detail: {h:?}"
            );
            assert!(
                h.detail.contains('·'),
                "expected tier · backend style detail: {h:?}"
            );
        }

        // Partial preferred still reorders without metadata loss; rest follow registry order.
        let partial = vec!["kimi-k2.5".into(), "qwen-plus".into()];
        let reordered = order_portable_hits(&partial, &registry);
        assert_eq!(reordered[0].id, "kimi-k2.5");
        assert_eq!(reordered[0].detail, "mid · moonshot");
        assert_eq!(reordered[1].id, "qwen-plus");
        assert_eq!(reordered[1].detail, "strong · dashscope");
        assert_eq!(reordered[2].id, "deepseek-v4-flash");
        assert_eq!(reordered[2].detail, "cheap · openrouter");
    }

    /// Mixed preferred: registry IDs keep metadata; unknown IDs are session-labeled.
    #[test]
    fn preferred_mix_registry_and_unknown() {
        let registry: Vec<(String, String)> =
            vec![("qwen-plus".into(), "strong · dashscope".into())];
        let preferred = vec!["custom-pin".into(), "qwen-plus".into()];
        let hits = order_portable_hits(&preferred, &registry);
        assert_eq!(hits[0].id, "custom-pin");
        assert!(hits[0].detail.contains("session"), "{:?}", hits[0]);
        assert_eq!(hits[1].id, "qwen-plus");
        assert_eq!(hits[1].detail, "strong · dashscope");
    }
}
