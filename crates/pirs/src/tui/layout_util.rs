//! Line wrap / clip helpers for chat drawing.
use ratatui::text::{Line, Span};

// ── Scroll helpers ──────────────────────────────────────────────────────────

pub(super) fn line_width(line: &Line) -> usize {
    line.spans
        .iter()
        .map(|s| unicode_width::UnicodeWidthStr::width(s.content.as_ref()))
        .sum()
}

/// Flatten logical lines into physical rows, each no wider than `width`, so the
/// row model is authoritative: the chat is rendered from these rows with no
/// further wrapping, so scroll math and paint can never disagree.
pub(super) fn flatten_rows(lines: &[Line<'static>], width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in lines {
        out.extend(wrap_line_to_rows(l, width));
    }
    out
}

/// Word-wrap one logical line into physical rows ≤ `width`, preserving span
/// styles. Overlong tokens (e.g. long paths) are hard-split; a space at a wrap
/// seam is dropped so continuation rows start on a word.
pub(super) fn wrap_line_to_rows(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    let w = width.max(1);
    if line_width(line) <= w {
        return vec![line.clone()];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    let mut cur_w = 0usize;

    for span in &line.spans {
        let style = span.style;
        for token in split_keep_spaces(span.content.as_ref()) {
            let tok_w = unicode_width::UnicodeWidthStr::width(token);
            if cur_w + tok_w <= w {
                cur.push(Span::styled(token.to_string(), style));
                cur_w += tok_w;
            } else if tok_w > w {
                // Token wider than a full row: hard-split by display columns.
                for ch in token.chars() {
                    let ch_w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                    if cur_w + ch_w > w && cur_w > 0 {
                        rows.push(Line::from(std::mem::take(&mut cur)));
                        cur_w = 0;
                    }
                    cur.push(Span::styled(ch.to_string(), style));
                    cur_w += ch_w;
                }
            } else {
                rows.push(Line::from(std::mem::take(&mut cur)));
                cur_w = 0;
                if token.chars().all(|c| c == ' ') {
                    continue; // drop the space that fell on the seam
                }
                cur.push(Span::styled(token.to_string(), style));
                cur_w = tok_w;
            }
        }
    }
    rows.push(Line::from(cur));
    rows
}

/// Split a string into alternating runs of spaces and non-spaces, keeping both.
pub(super) fn split_keep_spaces(s: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let mut start = 0usize;
    let mut cur_space: Option<bool> = None;
    for (i, ch) in s.char_indices() {
        let is_sp = ch == ' ';
        match cur_space {
            None => cur_space = Some(is_sp),
            Some(prev) if prev != is_sp => {
                out.push(&s[start..i]);
                start = i;
                cur_space = Some(is_sp);
            }
            _ => {}
        }
    }
    if start < s.len() {
        out.push(&s[start..]);
    }
    out
}

/// Clip a run of spans to `width` display columns, preserving each span's style
/// (unlike collapsing everything to one style after joining to a string).
pub(super) fn clip_spans(spans: Vec<Span<'static>>, width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut used = 0usize;
    for span in spans {
        if used >= width {
            break;
        }
        let sw = unicode_width::UnicodeWidthStr::width(span.content.as_ref());
        if used + sw <= width {
            used += sw;
            out.push(span);
        } else {
            let remaining = width - used;
            let mut buf = String::new();
            let mut bw = 0usize;
            for ch in span.content.chars() {
                let cw = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(1);
                if bw + cw > remaining {
                    break;
                }
                buf.push(ch);
                bw += cw;
            }
            if !buf.is_empty() {
                out.push(Span::styled(buf, span.style));
            }
            break;
        }
    }
    out
}

