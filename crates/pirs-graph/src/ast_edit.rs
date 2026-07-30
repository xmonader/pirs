use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::{json, Value};

use crate::graph::Lang;

#[derive(Deserialize, JsonSchema)]
struct AstEditArgs {
    /// Operation: replace_function_body | rename_symbol | move_function |
    /// insert_before_function | insert_after_function | list_functions
    op: String,
    /// File path containing the symbol (required except when op=list_functions still needs path)
    path: String,
    /// Symbol name (function for body/move/insert; any symbol for rename_symbol). Optional for list_functions.
    #[serde(default)]
    name: String,
    /// New body / new name / destination path / source text to insert (depending on op). Optional for list_functions.
    #[serde(default)]
    value: String,
}

pub struct AstEditTool {
    cwd: PathBuf,
}

impl AstEditTool {
    pub fn new(cwd: PathBuf) -> Self {
        AstEditTool { cwd }
    }
}

#[async_trait]
impl AgentTool for AstEditTool {
    fn name(&self) -> &str {
        "ast_edit"
    }

    fn description(&self) -> &str {
        "Symbol-level code edits (Rust, Python, TypeScript/TSX, Go) without fragile line numbers: \
         list_functions, replace_function_body (keeps signature), insert_before_function / \
         insert_after_function, rename_symbol (AST identifiers only), move_function. \
         Prefer over text edit for structural refactors; for project-wide rename use rename_symbol (LSP) tool."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AstEditArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "ast_edit: list_functions|replace_function_body|insert_before/after_function|\
             rename_symbol|move_function (rs/py/ts/go)",
        )
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: AstEditArgs = serde_json::from_value(ctx.args)?;
        let path = pirs_tools::paths::resolve_contained(&self.cwd, &args.path)?;
        // Serialize with edit/write on the same path (same process-wide filelock).
        let _mutation_guard = pirs_tools::filelock::lock(&path).await;
        let lang = Lang::from_path(&path)
            .filter(|l| {
                matches!(
                    l,
                    Lang::Rust | Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::Go
                )
            })
            .context("ast_edit supports Rust, Python, TypeScript/TSX, and Go files")?;

        let result = match args.op.as_str() {
            "list_functions" => list_functions(&path, lang)?,
            "replace_function_body" => {
                if args.name.is_empty() {
                    bail!("name required for replace_function_body");
                }
                replace_function_body(&path, lang, &args.name, &args.value)?
            }
            "insert_before_function" => {
                if args.name.is_empty() || args.value.is_empty() {
                    bail!("name and value (source text) required for insert_before_function");
                }
                insert_around_function(&path, lang, &args.name, &args.value, true)?
            }
            "insert_after_function" => {
                if args.name.is_empty() || args.value.is_empty() {
                    bail!("name and value (source text) required for insert_after_function");
                }
                insert_around_function(&path, lang, &args.name, &args.value, false)?
            }
            "rename_symbol" => {
                if args.name.is_empty() || args.value.is_empty() {
                    bail!("name (old) and value (new) required for rename_symbol");
                }
                rename_symbol(&path, lang, &args.name, &args.value)?
            }
            "move_function" => {
                if args.name.is_empty() || args.value.is_empty() {
                    bail!("name and value (dest path) required for move_function");
                }
                let dest = pirs_tools::paths::resolve_contained(&self.cwd, &args.value)?;
                move_function(&path, &dest, lang, &args.name)?
            }
            other => {
                bail!(
                    "unknown op '{other}': use list_functions|replace_function_body|\
                     insert_before_function|insert_after_function|rename_symbol|move_function"
                )
            }
        };

        Ok(ToolOutput::text(result.message).with_details(json!({
            "op": args.op,
            "path": path,
            "symbol": args.name,
            "firstChangedLine": result.first_line,
        })))
    }
}

struct EditResult {
    message: String,
    first_line: usize,
}

#[cfg(test)]
mod path_tests {
    #[test]
    fn ast_edit_rejects_path_outside_cwd() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let err = pirs_tools::paths::resolve_contained(cwd, "/etc/passwd");
        assert!(err.is_err(), "absolute escape must fail");
        let err2 = pirs_tools::paths::resolve_contained(cwd, "../../etc/passwd");
        assert!(err2.is_err(), "relative escape must fail");
    }
}

fn parse(lang: Lang, source: &str) -> anyhow::Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    let language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        Lang::TypeScript => tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into(),
        Lang::Tsx => tree_sitter_typescript::LANGUAGE_TSX.into(),
        Lang::Go => tree_sitter_go::LANGUAGE.into(),
    };
    parser.set_language(&language)?;
    parser.parse(source, None).context("failed to parse source")
}

fn function_kinds(lang: Lang) -> &'static [&'static str] {
    match lang {
        Lang::Rust => &["function_item"],
        Lang::Python => &["function_definition"],
        Lang::TypeScript | Lang::Tsx => &[
            "function_declaration",
            "method_definition",
            "generator_function_declaration",
        ],
        Lang::Go => &["function_declaration", "method_declaration"],
    }
}

fn find_function<'a>(
    tree: &'a tree_sitter::Tree,
    source: &'a str,
    lang: Lang,
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let kinds = function_kinds(lang);
    let mut cursor = tree.root_node().walk();
    find_fn_inner(tree.root_node(), source, &mut cursor, kinds, name)
}

fn find_fn_inner<'a>(
    node: tree_sitter::Node<'a>,
    source: &'a str,
    cursor: &mut tree_sitter::TreeCursor<'a>,
    target_kinds: &[&str],
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    if target_kinds.contains(&node.kind()) && function_name(node, source) == Some(name) {
        return Some(node);
    }
    if cursor.goto_first_child() {
        loop {
            if let Some(found) = find_fn_inner(cursor.node(), source, cursor, target_kinds, name) {
                return Some(found);
            }
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
    None
}

fn function_name<'a>(node: tree_sitter::Node<'a>, source: &'a str) -> Option<&'a str> {
    if let Some(n) = node.child_by_field_name("name") {
        return n.utf8_text(source.as_bytes()).ok();
    }
    // Go method_declaration: name is under field "name" usually; fallback scan
    let mut c = node.walk();
    if c.goto_first_child() {
        loop {
            let n = c.node();
            if matches!(
                n.kind(),
                "identifier" | "property_identifier" | "field_identifier"
            ) {
                if let Ok(t) = n.utf8_text(source.as_bytes()) {
                    return Some(t);
                }
            }
            if !c.goto_next_sibling() {
                break;
            }
        }
    }
    None
}

fn body_node<'a>(func: tree_sitter::Node<'a>, lang: Lang) -> Option<tree_sitter::Node<'a>> {
    match lang {
        Lang::Rust | Lang::Python | Lang::TypeScript | Lang::Tsx | Lang::Go => func
            .child_by_field_name("body")
            .or_else(|| func.child_by_field_name("statement_block")),
    }
}

fn list_functions(path: &Path, lang: Lang) -> anyhow::Result<EditResult> {
    let source = std::fs::read_to_string(path)?;
    let tree = parse(lang, &source)?;
    let kinds = function_kinds(lang);
    let mut names: Vec<(String, usize)> = Vec::new();
    let mut cursor = tree.root_node().walk();
    collect_functions(tree.root_node(), &source, &mut cursor, kinds, &mut names);
    if names.is_empty() {
        return Ok(EditResult {
            message: format!("no functions found in {}", path.display()),
            first_line: 1,
        });
    }
    let lines: Vec<String> = names
        .iter()
        .map(|(n, line)| format!("  L{line}: {n}"))
        .collect();
    Ok(EditResult {
        message: format!(
            "{} function(s) in {}:\n{}",
            names.len(),
            path.display(),
            lines.join("\n")
        ),
        first_line: names[0].1,
    })
}

fn collect_functions(
    node: tree_sitter::Node,
    source: &str,
    cursor: &mut tree_sitter::TreeCursor,
    kinds: &[&str],
    out: &mut Vec<(String, usize)>,
) {
    if kinds.contains(&node.kind()) {
        if let Some(n) = function_name(node, source) {
            out.push((n.to_string(), node.start_position().row + 1));
        }
    }
    if cursor.goto_first_child() {
        loop {
            collect_functions(cursor.node(), source, cursor, kinds, out);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

fn insert_around_function(
    path: &Path,
    lang: Lang,
    name: &str,
    text: &str,
    before: bool,
) -> anyhow::Result<EditResult> {
    let source = std::fs::read_to_string(path)?;
    let tree = parse(lang, &source)?;
    let func = find_function(&tree, &source, lang, name).with_context(|| {
        format!(
            "function '{name}' not found in {}. Use op=list_functions to see symbols.",
            path.display()
        )
    })?;
    let (span_start, span_end) = full_item_span(func, &source, lang);
    let insert = if text.ends_with('\n') {
        text.to_string()
    } else {
        format!("{text}\n")
    };
    let mut edited = source.clone();
    if before {
        edited.insert_str(span_start, &insert);
    } else {
        let mut at = span_end;
        if edited.as_bytes().get(at) == Some(&b'\n') {
            at += 1;
        }
        edited.insert_str(at, &insert);
    }
    write_with_rollback(path, &edited, lang)?;
    Ok(EditResult {
        message: format!(
            "Inserted {} function '{name}' in {}",
            if before { "before" } else { "after" },
            path.display()
        ),
        first_line: func.start_position().row + 1,
    })
}

fn reparse_check(path: &Path, lang: Lang) -> anyhow::Result<()> {
    let content = std::fs::read_to_string(path)?;
    let tree = parse(lang, &content)?;
    if tree.root_node().has_error() {
        bail!(
            "post-edit parse check failed: {} has syntax errors after mutation",
            path.display()
        );
    }
    Ok(())
}

fn write_with_rollback(path: &Path, content: &str, lang: Lang) -> anyhow::Result<()> {
    let original = std::fs::read_to_string(path)?;
    std::fs::write(path, content)?;
    if let Err(e) = reparse_check(path, lang) {
        let _ = std::fs::write(path, &original);
        return Err(e.context("edit rolled back"));
    }
    Ok(())
}

fn replace_function_body(
    path: &Path,
    lang: Lang,
    name: &str,
    new_body: &str,
) -> anyhow::Result<EditResult> {
    let source = std::fs::read_to_string(path)?;
    let tree = parse(lang, &source)?;
    let func = find_function(&tree, &source, lang, name)
        .with_context(|| format!("function '{name}' not found in {}", path.display()))?;
    let body = body_node(func, lang).context("function has no body node")?;

    let mut edited = source.clone();
    match lang {
        Lang::Rust | Lang::Go | Lang::TypeScript | Lang::Tsx => {
            // Brace languages: body is a block node including braces.
            let replacement = if new_body.trim_start().starts_with('{') {
                new_body.to_string()
            } else {
                format!("{{\n{new_body}\n}}")
            };
            edited.replace_range(body.start_byte()..body.end_byte(), &replacement);
        }
        Lang::Python => {
            // The body node starts at the first statement (after the indent);
            // replacing without a leading newline keeps exactly one indent.
            edited.replace_range(body.start_byte()..body.end_byte(), new_body.trim_end());
        }
    }
    write_with_rollback(path, &edited, lang)?;
    Ok(EditResult {
        message: format!(
            "Replaced body of {name} in {} ({} -> {} bytes)",
            path.display(),
            body.end_byte() - body.start_byte(),
            new_body.len()
        ),
        first_line: func.start_position().row + 1,
    })
}

fn rename_symbol(path: &Path, lang: Lang, old: &str, new: &str) -> anyhow::Result<EditResult> {
    if new.is_empty() || !new.chars().all(|c| c.is_alphanumeric() || c == '_') {
        bail!("new name must be a valid identifier");
    }
    let source = std::fs::read_to_string(path)?;
    let tree = parse(lang, &source)?;

    let mut spans: Vec<(usize, usize)> = Vec::new();
    let root = tree.root_node();
    let mut cursor = root.walk();
    collect_identifiers(root, &source, &mut cursor, old, &mut spans);
    if spans.is_empty() {
        bail!("symbol '{old}' not found in {}", path.display());
    }

    let mut edited = source.clone();
    for (start, end) in spans.iter().rev() {
        edited.replace_range(*start..*end, new);
    }
    write_with_rollback(path, &edited, lang)?;
    let first = line_of_byte(&source, spans[0].0);
    Ok(EditResult {
        message: format!(
            "Renamed '{old}' to '{new}' at {} site(s) in {}",
            spans.len(),
            path.display()
        ),
        first_line: first,
    })
}

fn collect_identifiers(
    node: tree_sitter::Node,
    source: &str,
    cursor: &mut tree_sitter::TreeCursor,
    name: &str,
    spans: &mut Vec<(usize, usize)>,
) {
    if matches!(
        node.kind(),
        "identifier"
            | "type_identifier"
            | "property_identifier"
            | "field_identifier"
            | "shorthand_property_identifier"
    ) && node.utf8_text(source.as_bytes()).unwrap_or("") == name
    {
        spans.push((node.start_byte(), node.end_byte()));
    }
    if cursor.goto_first_child() {
        loop {
            collect_identifiers(cursor.node(), source, cursor, name, spans);
            if !cursor.goto_next_sibling() {
                break;
            }
        }
        cursor.goto_parent();
    }
}

/// The byte span of a function *including* the attributes, decorators, and doc
/// comments bound to it. tree-sitter models these as the function's preceding
/// siblings (Rust `#[attr]` / `///`) or as a wrapping `decorated_definition`
/// (Python `@deco`); the bare `function_item`/`function_definition` node omits
/// them. Moving that node alone drops them from the destination and orphans
/// them in the source — an orphaned Python decorator is a syntax error.
fn full_item_span(func: tree_sitter::Node, source: &str, lang: Lang) -> (usize, usize) {
    if lang == Lang::Python {
        if let Some(parent) = func.parent() {
            if parent.kind() == "decorated_definition" {
                return (parent.start_byte(), parent.end_byte());
            }
        }
        return (func.start_byte(), func.end_byte());
    }
    if matches!(lang, Lang::TypeScript | Lang::Tsx) {
        // export function / async function: include export_statement parent when present
        if let Some(parent) = func.parent() {
            if parent.kind() == "export_statement" {
                return (parent.start_byte(), parent.end_byte());
            }
        }
        return (func.start_byte(), func.end_byte());
    }
    // Rust/Go: walk back over contiguous attribute / doc-comment siblings.
    let mut start = func.start_byte();
    let mut node = func;
    while let Some(prev) = node.prev_sibling() {
        let keep = match prev.kind() {
            "attribute_item" => true,
            "line_comment" | "block_comment" => {
                let t = prev.utf8_text(source.as_bytes()).unwrap_or("");
                t.starts_with("///")
                    || t.starts_with("//!")
                    || t.starts_with("/**")
                    || t.starts_with("/*!")
            }
            _ => false,
        };
        if !keep {
            break;
        }
        // A blank line between this sibling and the item below means it belongs
        // to something else (or nothing), not to our function — stop there.
        if source[prev.end_byte()..node.start_byte()]
            .matches('\n')
            .count()
            > 1
        {
            break;
        }
        start = prev.start_byte();
        node = prev;
    }
    (start, func.end_byte())
}

fn move_function(src: &Path, dest: &Path, lang: Lang, name: &str) -> anyhow::Result<EditResult> {
    // Same file: we write dest and then strip the function from src, so if they are the same
    // file the net effect is DELETING the function (M-21).
    //
    // Path comparison alone is not enough. The comment here used to say "same inode" while
    // comparing canonicalized PATHS — and `canonicalize` resolves symlinks but NOT hardlinks, so
    // two hardlinks to one inode compare unequal and slipped straight through. (There was also a
    // third clause that repeated the first verbatim, which clippy rejects as a logic bug — it was
    // dead, and removing it is what surfaced that the real check was missing.)
    let src_c = std::fs::canonicalize(src).unwrap_or_else(|_| src.to_path_buf());
    let dest_c = if dest.exists() {
        std::fs::canonicalize(dest).unwrap_or_else(|_| dest.to_path_buf())
    } else {
        dest.to_path_buf()
    };
    #[cfg(unix)]
    let same_inode = {
        use std::os::unix::fs::MetadataExt;
        match (std::fs::metadata(src), std::fs::metadata(dest)) {
            (Ok(a), Ok(b)) => a.dev() == b.dev() && a.ino() == b.ino(),
            _ => false,
        }
    };
    #[cfg(not(unix))]
    let same_inode = false;
    if src_c == dest_c || src == dest || same_inode {
        bail!(
            "move_function: destination is the same file as source ({}); \
             refusing (would delete the function)",
            src.display()
        );
    }
    let source = std::fs::read_to_string(src)?;
    let tree = parse(lang, &source)?;
    let func = find_function(&tree, &source, lang, name)
        .with_context(|| format!("function '{name}' not found in {}", src.display()))?;
    let (span_start, span_end) = full_item_span(func, &source, lang);
    let text = source[span_start..span_end].to_string();
    let first_line = source[..span_start].matches('\n').count() + 1;

    // Write the destination FIRST: if it fails to parse, roll it back to its
    // prior state and leave the source untouched.
    let dest_existed = dest.exists();
    let dest_original = if dest_existed {
        std::fs::read_to_string(dest)?
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        String::new()
    };
    let mut dest_content = dest_original.clone();
    if !dest_content.is_empty() && !dest_content.ends_with('\n') {
        dest_content.push('\n');
    }
    dest_content.push('\n');
    dest_content.push_str(text.trim_end());
    dest_content.push('\n');
    std::fs::write(dest, &dest_content)?;
    if let Err(e) = reparse_check(dest, lang) {
        restore(dest, dest_existed, &dest_original);
        return Err(e.context("edit rolled back"));
    }

    let mut remaining = source.clone();
    let end = if source.as_bytes().get(span_end) == Some(&b'\n') {
        span_end + 1
    } else {
        span_end
    };
    remaining.replace_range(span_start..end, "");
    std::fs::write(src, &remaining)?;
    // If removing the item leaves the source unparseable, both files must be
    // restored — the destination was already written above.
    if let Err(e) = reparse_check(src, lang) {
        let _ = std::fs::write(src, &source);
        restore(dest, dest_existed, &dest_original);
        return Err(e.context("edit rolled back"));
    }

    Ok(EditResult {
        message: format!("Moved {name} from {} to {}", src.display(), dest.display()),
        first_line,
    })
}

fn restore(path: &Path, existed: bool, original: &str) {
    if existed {
        let _ = std::fs::write(path, original);
    } else {
        let _ = std::fs::remove_file(path);
    }
}

fn line_of_byte(source: &str, byte: usize) -> usize {
    source[..byte].lines().count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn tool(dir: &Path) -> AstEditTool {
        AstEditTool::new(dir.to_path_buf())
    }

    async fn run(t: &AstEditTool, args: Value) -> anyhow::Result<ToolOutput> {
        t.execute(ToolExecContext {
            tool_call_id: "t".into(),
            args,
            cancel: CancellationToken::new(),
            on_update: None,
        })
        .await
    }

    #[tokio::test]
    async fn replace_body_rust() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn add(a: i32, b: i32) -> i32 {\n    0\n}\n").unwrap();
        let out = run(
            &tool(dir.path()),
            json!({"op": "replace_function_body", "path": "a.rs", "name": "add", "value": "    a + b"}),
        )
        .await
        .unwrap();
        assert!(out.content[0]
            .as_text()
            .unwrap()
            .contains("Replaced body of add"));
        let content = std::fs::read_to_string(&f).unwrap();
        assert_eq!(content, "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
    }

    #[tokio::test]
    async fn replace_body_python() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.py");
        std::fs::write(&f, "def add(a, b):\n    return 0\n").unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "replace_function_body", "path": "a.py", "name": "add", "value": "return a + b"}),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(&f).unwrap();
        assert_eq!(content, "def add(a, b):\n    return a + b\n");
    }

    #[tokio::test]
    async fn rename_symbol_ast_precise() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(
            &f,
            "fn process() { process_inner(); }\nfn process_inner() {}\n// process docs\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "rename_symbol", "path": "a.rs", "name": "process_inner", "value": "handle"}),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(&f).unwrap();
        assert_eq!(
            content,
            "fn process() { handle(); }\nfn handle() {}\n// process docs\n"
        );
    }

    #[tokio::test]
    async fn move_function_between_files() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(&a, "fn keep() {}\nfn gone() { 1; }\n").unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "move_function", "path": "a.rs", "name": "gone", "value": "b.rs"}),
        )
        .await
        .unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "fn keep() {}\n");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "\nfn gone() { 1; }\n");
    }

    #[tokio::test]
    async fn move_function_carries_rust_attributes_and_docs() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.rs");
        let b = dir.path().join("b.rs");
        std::fs::write(
            &a,
            "fn keep() {}\n/// docs for gone\n#[inline]\nfn gone() { 1; }\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "move_function", "path": "a.rs", "name": "gone", "value": "b.rs"}),
        )
        .await
        .unwrap();
        // The attribute and doc comment travel with the function; the source is
        // left with neither orphaned.
        let a_after = std::fs::read_to_string(&a).unwrap();
        assert_eq!(a_after, "fn keep() {}\n");
        let b_after = std::fs::read_to_string(&b).unwrap();
        assert!(b_after.contains("/// docs for gone"), "docs: {b_after:?}");
        assert!(b_after.contains("#[inline]"), "attr: {b_after:?}");
        assert!(b_after.contains("fn gone() { 1; }"));
    }

    #[test]
    fn move_function_rejects_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn foo() { 1 }\n").unwrap();
        let err = match move_function(&f, &f, Lang::Rust, "foo") {
            Ok(_) => panic!("expected same-file reject"),
            Err(e) => e.to_string(),
        };
        assert!(
            err.contains("same file") || err.contains("refusing"),
            "{err}"
        );
        let src = std::fs::read_to_string(&f).unwrap();
        assert!(src.contains("fn foo"), "must not delete: {src}");
    }

    #[tokio::test]
    async fn move_function_carries_python_decorators() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.py");
        let b = dir.path().join("b.py");
        std::fs::write(
            &a,
            "def keep():\n    pass\n\n@staticmethod\ndef gone():\n    return 1\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "move_function", "path": "a.py", "name": "gone", "value": "b.py"}),
        )
        .await
        .unwrap();
        // Neither file may end up with an orphaned decorator (a syntax error).
        let a_after = std::fs::read_to_string(&a).unwrap();
        assert!(!a_after.contains("@staticmethod"), "orphaned: {a_after:?}");
        let b_after = std::fs::read_to_string(&b).unwrap();
        assert!(b_after.contains("@staticmethod"), "decorator: {b_after:?}");
        assert!(b_after.contains("def gone():"));
    }

    #[tokio::test]
    async fn missing_function_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn x() {}\n").unwrap();
        let err = run(
            &tool(dir.path()),
            json!({"op": "replace_function_body", "path": "a.rs", "name": "nope", "value": "1"}),
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not found"));
    }

    #[tokio::test]
    async fn list_functions_rust() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn alpha() {}\nfn beta() {}\n").unwrap();
        let out = run(
            &tool(dir.path()),
            json!({"op": "list_functions", "path": "a.rs"}),
        )
        .await
        .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("alpha"), "{text}");
        assert!(text.contains("beta"), "{text}");
        assert!(text.contains("2 function"), "{text}");
    }

    #[tokio::test]
    async fn insert_before_and_after_function() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.rs");
        std::fs::write(&f, "fn mid() { 1 }\n").unwrap();
        run(
            &tool(dir.path()),
            json!({
                "op": "insert_before_function",
                "path": "a.rs",
                "name": "mid",
                "value": "fn before() {}"
            }),
        )
        .await
        .unwrap();
        run(
            &tool(dir.path()),
            json!({
                "op": "insert_after_function",
                "path": "a.rs",
                "name": "mid",
                "value": "fn after() {}"
            }),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(&f).unwrap();
        assert!(content.contains("fn before()"), "{content}");
        assert!(content.contains("fn mid()"), "{content}");
        assert!(content.contains("fn after()"), "{content}");
        let b = content.find("fn before()").unwrap();
        let m = content.find("fn mid()").unwrap();
        let a = content.find("fn after()").unwrap();
        assert!(b < m && m < a, "order wrong: {content}");
    }

    #[tokio::test]
    async fn replace_body_typescript() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.ts");
        std::fs::write(
            &f,
            "function add(a: number, b: number): number {\n  return 0;\n}\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({
                "op": "replace_function_body",
                "path": "a.ts",
                "name": "add",
                "value": "  return a + b;"
            }),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(&f).unwrap();
        assert!(content.contains("return a + b"), "{content}");
        assert!(content.contains("function add"), "{content}");
    }

    #[tokio::test]
    async fn replace_body_go() {
        let dir = tempfile::tempdir().unwrap();
        let f = dir.path().join("a.go");
        std::fs::write(
            &f,
            "package main\n\nfunc Add(a int, b int) int {\n\treturn 0\n}\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({
                "op": "replace_function_body",
                "path": "a.go",
                "name": "Add",
                "value": "\treturn a + b"
            }),
        )
        .await
        .unwrap();
        let content = std::fs::read_to_string(&f).unwrap();
        assert!(content.contains("return a + b"), "{content}");
        assert!(content.contains("func Add"), "{content}");
    }

    #[tokio::test]
    async fn list_functions_go_and_ts() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("a.go"),
            "package main\n\nfunc Foo() {}\nfunc Bar() {}\n",
        )
        .unwrap();
        std::fs::write(
            dir.path().join("a.ts"),
            "function one() {}\nexport function two() {}\n",
        )
        .unwrap();
        let go = run(
            &tool(dir.path()),
            json!({"op": "list_functions", "path": "a.go"}),
        )
        .await
        .unwrap();
        let go_t = go.content[0].as_text().unwrap();
        assert!(go_t.contains("Foo") && go_t.contains("Bar"), "{go_t}");
        let ts = run(
            &tool(dir.path()),
            json!({"op": "list_functions", "path": "a.ts"}),
        )
        .await
        .unwrap();
        let ts_t = ts.content[0].as_text().unwrap();
        assert!(ts_t.contains("one") && ts_t.contains("two"), "{ts_t}");
    }

    #[tokio::test]
    async fn move_export_function_typescript() {
        let dir = tempfile::tempdir().unwrap();
        let a = dir.path().join("a.ts");
        let b = dir.path().join("b.ts");
        std::fs::write(
            &a,
            "function keep() {}\nexport function gone() {\n  return 1;\n}\n",
        )
        .unwrap();
        run(
            &tool(dir.path()),
            json!({"op": "move_function", "path": "a.ts", "name": "gone", "value": "b.ts"}),
        )
        .await
        .unwrap();
        let a_after = std::fs::read_to_string(&a).unwrap();
        assert!(!a_after.contains("gone"), "{a_after}");
        assert!(a_after.contains("keep"), "{a_after}");
        let b_after = std::fs::read_to_string(&b).unwrap();
        assert!(b_after.contains("export function gone"), "{b_after}");
    }
}
