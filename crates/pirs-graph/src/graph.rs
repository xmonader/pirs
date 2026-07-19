use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SymKind {
    Function,
    Method,
    Struct,
    Class,
    Trait,
    Enum,
    Module,
}

impl SymKind {
    pub fn name(&self) -> &'static str {
        match self {
            SymKind::Function => "fn",
            SymKind::Method => "method",
            SymKind::Struct => "struct",
            SymKind::Class => "class",
            SymKind::Trait => "trait",
            SymKind::Enum => "enum",
            SymKind::Module => "module",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Symbol {
    pub name: String,
    pub kind: SymKind,
    pub file: PathBuf,
    pub line: usize,
    pub start_byte: usize,
    pub end_byte: usize,
    pub calls: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    Rust,
    Python,
    TypeScript,
    Tsx,
    Go,
}

impl Lang {
    pub fn from_path(path: &Path) -> Option<Lang> {
        match path.extension().and_then(|e| e.to_str())? {
            "rs" => Some(Lang::Rust),
            "py" => Some(Lang::Python),
            "ts" => Some(Lang::TypeScript),
            "tsx" | "jsx" => Some(Lang::Tsx),
            "go" => Some(Lang::Go),
            _ => None,
        }
    }

    fn grammar(self) -> tree_sitter::Language {
        match self {
            Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
            Lang::Python => tree_sitter_python::LANGUAGE.into(),
            Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
            Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
            Lang::Go => tree_sitter_go::LANGUAGE.into(),
        }
    }
}

#[derive(Default)]
pub struct Graph {
    pub symbols: Vec<Symbol>,
    by_name: HashMap<String, Vec<usize>>,
    by_file: HashMap<PathBuf, Vec<usize>>,
    refs: HashMap<String, Vec<usize>>,
    pagerank: HashMap<String, f64>,
}

impl Graph {
    pub fn build(root: &Path) -> Graph {
        Graph::from_symbols(parse_tree(root))
    }

    /// Build the query indexes (`by_name`/`by_file`/`refs`) and PageRank over a
    /// flat symbol set. Both the full parse (`build`) and the persistent
    /// incremental store feed through here, so a from-scratch graph and an
    /// incrementally-refreshed one over the same symbol set are byte-for-byte the
    /// same structure — the equivalence guarantee lives in this one function.
    pub fn from_symbols(symbols: Vec<Symbol>) -> Graph {
        let mut by_name: HashMap<String, Vec<usize>> = HashMap::new();
        let mut by_file: HashMap<PathBuf, Vec<usize>> = HashMap::new();
        let mut refs: HashMap<String, Vec<usize>> = HashMap::new();
        for (i, sym) in symbols.iter().enumerate() {
            by_name.entry(sym.name.clone()).or_default().push(i);
            by_file.entry(sym.file.clone()).or_default().push(i);
            for callee in &sym.calls {
                refs.entry(callee.clone()).or_default().push(i);
            }
        }
        let mut graph = Graph {
            symbols,
            by_name,
            by_file,
            refs,
            pagerank: HashMap::new(),
        };
        graph.compute_pagerank();
        graph
    }

    fn compute_pagerank(&mut self) {
        const DAMPING: f64 = 0.85;
        const ITERS: usize = 20;
        let n = self.symbols.len();
        if n == 0 {
            return;
        }
        let mut rank = vec![1.0 / n as f64; n];
        let mut outgoing: HashMap<usize, Vec<usize>> = HashMap::new();
        for (i, sym) in self.symbols.iter().enumerate() {
            let mut targets = Vec::new();
            for callee in &sym.calls {
                if let Some(idxs) = self.by_name.get(callee) {
                    for &t in idxs {
                        if t != i && !targets.contains(&t) {
                            targets.push(t);
                        }
                    }
                }
            }
            outgoing.insert(i, targets);
        }
        for _ in 0..ITERS {
            let mut next = vec![(1.0 - DAMPING) / n as f64; n];
            for (i, targets) in &outgoing {
                if targets.is_empty() {
                    let share = DAMPING * rank[*i] / n as f64;
                    for v in next.iter_mut() {
                        *v += share;
                    }
                } else {
                    let share = DAMPING * rank[*i] / targets.len() as f64;
                    for &t in targets {
                        next[t] += share;
                    }
                }
            }
            rank = next;
        }
        for (i, sym) in self.symbols.iter().enumerate() {
            self.pagerank.insert(sym.name.clone(), rank[i]);
        }
    }

    pub fn symbol(&self, name: &str) -> Vec<&Symbol> {
        self.by_name
            .get(name)
            .map(|idxs| idxs.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    pub fn callers(&self, name: &str) -> Vec<&Symbol> {
        self.refs
            .get(name)
            .map(|idxs| idxs.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    pub fn callees(&self, name: &str) -> Vec<String> {
        let mut out: Vec<String> = self
            .symbol(name)
            .into_iter()
            .flat_map(|s| s.calls.clone())
            .collect();
        out.sort();
        out.dedup();
        out
    }

    pub fn file_symbols(&self, path: &Path) -> Vec<&Symbol> {
        self.by_file
            .get(path)
            .map(|idxs| idxs.iter().map(|&i| &self.symbols[i]).collect())
            .unwrap_or_default()
    }

    /// Names of test functions that directly call a symbol defined in `path`.
    /// Deliberately shallow (direct callers only): transitive closure over a
    /// name-based graph explodes until you're re-running half the suite. A
    /// symbol counts as a test when its name starts with `test` or it lives
    /// in a `tests/` path or a `*_test.rs` file.
    pub fn affected_tests(&self, path: &Path) -> Vec<String> {
        fn is_test(s: &Symbol) -> bool {
            s.name.starts_with("test")
                || s.file.components().any(|c| c.as_os_str() == "tests")
                || s.file
                    .file_name()
                    .and_then(|f| f.to_str())
                    .map(|f| f.ends_with("_test.rs"))
                    .unwrap_or(false)
        }
        let mut out: Vec<String> = self
            .file_symbols(path)
            .into_iter()
            .flat_map(|s| self.callers(&s.name))
            .filter(|c| is_test(c))
            .map(|c| c.name.clone())
            .collect();
        // Tests defined in the edited file itself are always affected.
        out.extend(
            self.file_symbols(path)
                .into_iter()
                .filter(|s| is_test(s))
                .map(|s| s.name.clone()),
        );
        out.sort();
        out.dedup();
        out
    }

    pub fn top(&self, n: usize) -> Vec<(&Symbol, f64)> {
        let mut scored: Vec<(&Symbol, f64)> = self
            .symbols
            .iter()
            .map(|s| (s, *self.pagerank.get(&s.name).unwrap_or(&0.0)))
            .collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        scored.truncate(n);
        scored
    }

    pub fn find_definition(&self, name: &str, file: &Path) -> Option<&Symbol> {
        self.symbol(name).into_iter().find(|s| s.file == file)
    }
}

/// Walk `root` (gitignore-aware) and parse every supported source file into a
/// flat symbol set, in filesystem-walk order.
pub fn parse_tree(root: &Path) -> Vec<Symbol> {
    let mut symbols = Vec::new();
    let walker = ignore::WalkBuilder::new(root)
        .hidden(false)
        .require_git(false)
        .build();
    for entry in walker.flatten() {
        let path = entry.path();
        if path.is_dir() {
            continue;
        }
        if let Some(mut file_symbols) = parse_file(path) {
            symbols.append(&mut file_symbols);
        }
    }
    symbols
}

/// Parse a single file into its symbols, or `None` if it is not a supported
/// language or cannot be read/parsed. This is the unit the persistent store
/// caches: re-run it only for files whose bytes changed.
pub fn parse_file(path: &Path) -> Option<Vec<Symbol>> {
    let lang = Lang::from_path(path)?;
    let source = std::fs::read_to_string(path).ok()?;
    let mut parser = tree_sitter::Parser::new();
    if parser.set_language(&lang.grammar()).is_err() {
        return None;
    }
    let tree = parser.parse(&source, None)?;
    Some(extract_symbols(lang, tree.root_node(), &source, path))
}

fn extract_symbols(lang: Lang, root: tree_sitter::Node, source: &str, path: &Path) -> Vec<Symbol> {
    let mut out = Vec::new();
    let mut cursor = root.walk();
    walk_symbols(lang, root, source, path, &mut cursor, &mut out, 0);
    out
}

/// Depth ceiling for the AST walkers. tree-sitter parses pathologically nested
/// input (e.g. 50k open parens) without complaint, and Graph::build walks every
/// repo file, so unbounded recursion lets one adversarial file stack-overflow
/// and abort the whole agent. Real source never approaches this.
const MAX_WALK_DEPTH: usize = 512;

fn walk_symbols(
    lang: Lang,
    node: tree_sitter::Node,
    source: &str,
    path: &Path,
    cursor: &mut tree_sitter::TreeCursor,
    out: &mut Vec<Symbol>,
    depth: usize,
) {
    let go_deeper = depth < MAX_WALK_DEPTH;
    if let Some((kind, name_node)) = definition_info(lang, &node) {
        let name = node_text(name_node, source).to_string();
        if !name.is_empty() {
            let calls = if matches!(kind, SymKind::Function | SymKind::Method) {
                collect_calls(lang, node, source)
            } else {
                Vec::new()
            };
            out.push(Symbol {
                name,
                kind,
                file: path.to_path_buf(),
                line: node.start_position().row + 1,
                start_byte: node.start_byte(),
                end_byte: node.end_byte(),
                calls,
            });
        }
    }
    if go_deeper && cursor.goto_first_child() {
        loop {
            walk_symbols(lang, cursor.node(), source, path, cursor, out, depth + 1);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn definition_info<'a>(
    lang: Lang,
    node: &tree_sitter::Node<'a>,
) -> Option<(SymKind, tree_sitter::Node<'a>)> {
    let kind = node.kind();
    match (lang, kind) {
        (Lang::Rust, "function_item") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Function, n)),
        (Lang::Rust, "struct_item") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Struct, n)),
        (Lang::Rust, "enum_item") => node.child_by_field_name("name").map(|n| (SymKind::Enum, n)),
        (Lang::Rust, "trait_item") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Trait, n)),
        (Lang::Rust, "mod_item") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Module, n)),
        (Lang::Python, "function_definition") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Function, n)),
        (Lang::Python, "class_definition") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Class, n)),
        (Lang::TypeScript | Lang::Tsx, "function_declaration") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Function, n)),
        (Lang::TypeScript | Lang::Tsx, "method_definition") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Method, n)),
        (Lang::TypeScript | Lang::Tsx, "class_declaration") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Class, n)),
        (Lang::Go, "function_declaration") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Function, n)),
        (Lang::Go, "method_declaration") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Method, n)),
        (Lang::Go, "type_declaration") => node
            .child_by_field_name("name")
            .map(|n| (SymKind::Struct, n)),
        _ => None,
    }
}

fn collect_calls(lang: Lang, func: tree_sitter::Node, source: &str) -> Vec<String> {
    let mut calls = Vec::new();
    let mut cursor = func.walk();
    collect_calls_inner(lang, func, source, &mut cursor, &mut calls, 0);
    calls.sort();
    calls.dedup();
    calls
}

fn collect_calls_inner(
    lang: Lang,
    node: tree_sitter::Node,
    source: &str,
    cursor: &mut tree_sitter::TreeCursor,
    calls: &mut Vec<String>,
    depth: usize,
) {
    if depth >= MAX_WALK_DEPTH {
        return;
    }
    let kind = node.kind();
    let is_call = matches!(
        (lang, kind),
        (Lang::Rust, "call_expression")
            | (Lang::Python, "call")
            | (Lang::TypeScript | Lang::Tsx, "call_expression")
            | (Lang::Go, "call_expression")
    );
    if is_call {
        if let Some(f) = node.child_by_field_name("function") {
            if let Some(name) = callee_name(f, source) {
                calls.push(name);
            }
        }
    }
    if cursor.goto_first_child() {
        loop {
            collect_calls_inner(lang, cursor.node(), source, cursor, calls, depth + 1);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn callee_name<'a>(node: tree_sitter::Node<'a>, source: &'a str) -> Option<String> {
    match node.kind() {
        "identifier" => Some(node_text(node, source).to_string()),
        "field_expression" | "attribute" | "member_expression" | "selector_expression" => node
            .child_by_field_name("field")
            .or_else(|| node.child_by_field_name("property"))
            .or_else(|| node.child_by_field_name("attribute"))
            .or_else(|| node.named_child(node.named_child_count().saturating_sub(1)))
            .map(|n| node_text(n, source).to_string()),
        _ => None,
    }
}

fn node_text<'a>(node: tree_sitter::Node<'a>, source: &'a str) -> &'a str {
    node.utf8_text(source.as_bytes()).unwrap_or("")
}
