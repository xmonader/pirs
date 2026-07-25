//! UTF-8-safe message chunking.

/// Split `s` into chunks of at most `max_chars` Unicode scalars.
pub fn utf8_chunks(s: &str, max_chars: usize) -> Vec<String> {
    if s.is_empty() {
        return vec![String::new()];
    }
    let mut out = Vec::new();
    let mut cur = String::new();
    for ch in s.chars() {
        if cur.chars().count() >= max_chars {
            out.push(std::mem::take(&mut cur));
        }
        cur.push(ch);
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}
