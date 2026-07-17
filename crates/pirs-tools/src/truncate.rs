pub const MAX_LINES: usize = 2000;
pub const MAX_BYTES: usize = 50 * 1024;
pub const GREP_LINE_MAX: usize = 500;

#[derive(Debug, PartialEq)]
pub struct Window {
    pub text: String,
    pub total_lines: usize,
    pub start_line: usize,
    pub end_line: usize,
    pub truncated: bool,
}

pub fn head(content: &str, offset: usize, limit: usize) -> Window {
    let offset = offset.max(1);
    let total_lines = content.lines().count();
    let mut text = String::new();
    let mut end_line = offset - 1;
    for (count, (i, line)) in content.lines().enumerate().skip(offset - 1).enumerate() {
        if count >= limit || text.len() + line.len() + 1 > MAX_BYTES {
            break;
        }
        text.push_str(line);
        text.push('\n');
        end_line = i + 1;
    }
    Window {
        text,
        total_lines,
        start_line: offset,
        end_line,
        truncated: end_line < total_lines,
    }
}

pub fn tail(content: &str, limit: usize) -> Window {
    let total_lines = content.lines().count();
    let mut lines: Vec<&str> = content.lines().collect();
    if total_lines > limit {
        lines = lines.split_off(total_lines - limit);
    }
    let mut size: usize = lines.iter().map(|l| l.len() + 1).sum();
    while !lines.is_empty() && size > MAX_BYTES {
        size -= lines[0].len() + 1;
        lines.remove(0);
    }
    let mut text = String::new();
    for line in &lines {
        text.push_str(line);
        text.push('\n');
    }
    let start = total_lines - lines.len() + 1;
    Window {
        text,
        total_lines,
        start_line: start,
        end_line: total_lines,
        truncated: start > 1,
    }
}

pub fn truncate_line(line: &str, max: usize) -> String {
    if line.len() <= max {
        line.to_string()
    } else {
        let mut end = max;
        while !line.is_char_boundary(end) {
            end -= 1;
        }
        format!("{}...", &line[..end])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn head_basic() {
        let content = "a\nb\nc\nd\n";
        let w = head(content, 2, 2);
        assert_eq!(w.text, "b\nc\n");
        assert_eq!(w.start_line, 2);
        assert_eq!(w.end_line, 3);
        assert!(w.truncated);
        assert_eq!(w.total_lines, 4);
    }

    #[test]
    fn head_no_truncation() {
        let w = head("a\nb\n", 1, 10);
        assert!(!w.truncated);
        assert_eq!(w.text, "a\nb\n");
    }

    #[test]
    fn tail_basic() {
        let content = (1..=10).map(|i| i.to_string()).collect::<Vec<_>>().join("\n");
        let w = tail(&content, 3);
        assert_eq!(w.text, "8\n9\n10\n");
        assert_eq!(w.start_line, 8);
        assert!(w.truncated);
    }

    #[test]
    fn truncate_line_multibyte() {
        let s = "héllo wörld";
        let t = truncate_line(s, 4);
        assert!(t.ends_with("..."));
    }
}
