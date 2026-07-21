//! Compact repo-map sketch for the system prompt.
//!
//! Renders PageRank-ordered symbols as a short, file-grouped outline so the
//! model sees structure without a tool call. Budget is character-based (no
//! tokenizer); callers typically pass 3–6k for weak models and ~4k default.

use std::collections::BTreeMap;
use std::path::Path;

use crate::graph::{Graph, SymKind};

/// Default sketch budget (chars) — enough for top symbols without drowning context.
pub const DEFAULT_MAP_CHARS: usize = 4_000;

/// Render a PageRank-ranked, file-grouped symbol sketch of the graph.
///
/// Format (file-grouped, rank-ordered listing):
/// ```text
/// <repo_map>
/// src/lib.rs:
///   fn add
///   fn mul
/// src/main.rs:
///   fn main
/// </repo_map>
/// ```
///
/// Symbols are emitted best-first by pagerank; when the budget fills, lower-
/// ranked symbols are dropped. Returns `None` when the graph is empty.
pub fn render_sketch(graph: &Graph, root: &Path, max_chars: usize) -> Option<String> {
    if graph.symbols.is_empty() || max_chars < 64 {
        return None;
    }

    // Group ranked symbols by relative file path, preserving first-seen order
    // of files (highest-ranked symbol's file first).
    let ranked = graph.ranked_symbols();
    let mut file_order: Vec<String> = Vec::new();
    let mut by_file: BTreeMap<String, Vec<(SymKind, String, f64)>> = BTreeMap::new();

    for (sym, rank) in ranked {
        let rel = sym
            .file
            .strip_prefix(root)
            .unwrap_or(&sym.file)
            .to_string_lossy()
            .replace('\\', "/");
        // Skip noise: very short private-looking names and test-only noise later.
        if sym.name.starts_with('_') && sym.name.len() < 4 {
            continue;
        }
        let entry = by_file.entry(rel.clone()).or_default();
        // One entry per name per file (keep best rank).
        if entry.iter().any(|(_, n, _)| n == &sym.name) {
            continue;
        }
        if !file_order.iter().any(|f| f == &rel) {
            file_order.push(rel);
        }
        entry.push((sym.kind, sym.name.clone(), rank));
    }

    if file_order.is_empty() {
        return None;
    }

    let mut out = String::from("<repo_map>\n");
    out.push_str(
        "# Ranked symbols (PageRank). Prefer code_map/code_search for details.\n",
    );

    let header_len = out.len();
    let mut used = header_len;
    let footer = "</repo_map>\n";
    let budget = max_chars.saturating_sub(footer.len() + 8);

    for rel in &file_order {
        let Some(syms) = by_file.get(rel) else {
            continue;
        };
        // Sort within file by rank desc.
        let mut syms = syms.clone();
        syms.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));

        let file_header = format!("{rel}:\n");
        if used + file_header.len() > budget {
            break;
        }
        out.push_str(&file_header);
        used += file_header.len();

        for (n_in_file, (kind, name, _rank)) in syms.iter().enumerate() {
            // Cap symbols per file so one giant module doesn't monopolize the map.
            if n_in_file >= 12 {
                break;
            }
            let line = format!("  {} {name}\n", kind.name());
            if used + line.len() > budget {
                // Close early with an ellipsis note if we have anything useful.
                if out.len() > header_len + 20 {
                    out.push_str("  …\n");
                }
                out.push_str(footer);
                return Some(out);
            }
            out.push_str(&line);
            used += line.len();
        }
    }

    out.push_str(footer);
    // Need at least one symbol line to be useful.
    if out.lines().count() < 4 {
        return None;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    #[test]
    fn sketch_includes_high_rank_callees() {
        let dir = tempfile::tempdir().unwrap();
        // b is called by a and c → higher rank.
        std::fs::write(
            dir.path().join("lib.rs"),
            "fn a() { b(); }\nfn b() {}\nfn c() { b(); }\n",
        )
        .unwrap();
        let g = Graph::build(dir.path());
        let sketch = render_sketch(&g, dir.path(), 2000).expect("sketch");
        assert!(sketch.contains("lib.rs") || sketch.contains("fn b"), "{sketch}");
        assert!(sketch.contains("<repo_map>"));
        assert!(sketch.contains("</repo_map>"));
    }

    #[test]
    fn sketch_respects_budget() {
        let dir = tempfile::tempdir().unwrap();
        let mut f = std::fs::File::create(dir.path().join("big.rs")).unwrap();
        for i in 0..200 {
            writeln!(f, "fn f{i}() {{}}").unwrap();
        }
        let g = Graph::build(dir.path());
        let sketch = render_sketch(&g, dir.path(), 400).expect("sketch");
        assert!(sketch.len() <= 450, "len={}", sketch.len());
        assert!(sketch.contains("</repo_map>"));
    }

    #[test]
    fn empty_graph_returns_none() {
        let dir = tempfile::tempdir().unwrap();
        let g = Graph::build(dir.path());
        assert!(render_sketch(&g, dir.path(), 4000).is_none());
    }
}
