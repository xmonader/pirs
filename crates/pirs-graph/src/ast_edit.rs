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
    /// Operation: replace_function_body | rename_symbol | move_function
    op: String,
    /// File path containing the symbol
    path: String,
    /// Symbol name (function for replace_function_body/move_function; any symbol for rename_symbol)
    name: String,
    /// New body (replace_function_body) or new name (rename_symbol) or destination file (move_function)
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
        "Edit code at the symbol level (Rust and Python): replace_function_body (keeps the signature), rename_symbol (AST-precise, no string-match accidents), move_function to another file. Safer than text edit for structural changes."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AstEditArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("ast_edit: symbol-level edits (replace function body, rename, move) — prefer over edit for refactors")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: AstEditArgs = serde_json::from_value(ctx.args)?;
        let path = resolve(&self.cwd, &args.path);
        let lang = Lang::from_path(&path)
            .filter(|l| matches!(l, Lang::Rust | Lang::Python))
            .context("ast_edit supports Rust and Python files")?;

        let result = match args.op.as_str() {
            "replace_function_body" => replace_function_body(&path, lang, &args.name, &args.value)?,
            "rename_symbol" => rename_symbol(&path, lang, &args.name, &args.value)?,
            "move_function" => {
                let dest = resolve(&self.cwd, &args.value);
                move_function(&path, &dest, lang, &args.name)?
            }
            other => {
                bail!("unknown op '{other}': use replace_function_body|rename_symbol|move_function")
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

fn resolve(cwd: &Path, input: &str) -> PathBuf {
    let p = Path::new(input);
    if p.is_absolute() {
        p.to_path_buf()
    } else {
        cwd.join(p)
    }
}

fn parse(lang: Lang, source: &str) -> anyhow::Result<tree_sitter::Tree> {
    let mut parser = tree_sitter::Parser::new();
    let language = match lang {
        Lang::Rust => tree_sitter_rust::LANGUAGE.into(),
        Lang::Python => tree_sitter_python::LANGUAGE.into(),
        _ => bail!("unsupported language for ast_edit"),
    };
    parser.set_language(&language)?;
    parser.parse(source, None).context("failed to parse source")
}

fn find_function<'a>(
    tree: &'a tree_sitter::Tree,
    source: &'a str,
    lang: Lang,
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    let target_kind = match lang {
        Lang::Rust => "function_item",
        Lang::Python => "function_definition",
        _ => return None,
    };
    let mut cursor = tree.root_node().walk();
    find_fn_inner(tree.root_node(), source, &mut cursor, target_kind, name)
}

fn find_fn_inner<'a>(
    node: tree_sitter::Node<'a>,
    source: &'a str,
    cursor: &mut tree_sitter::TreeCursor<'a>,
    target_kind: &str,
    name: &str,
) -> Option<tree_sitter::Node<'a>> {
    if node.kind() == target_kind {
        if let Some(n) = node.child_by_field_name("name") {
            if n.utf8_text(source.as_bytes()).unwrap_or("") == name {
                return Some(node);
            }
        }
    }
    if cursor.goto_first_child() {
        loop {
            if let Some(found) = find_fn_inner(cursor.node(), source, cursor, target_kind, name) {
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

fn body_node<'a>(func: tree_sitter::Node<'a>, lang: Lang) -> Option<tree_sitter::Node<'a>> {
    match lang {
        Lang::Rust => func.child_by_field_name("body"),
        Lang::Python => func.child_by_field_name("body"),
        _ => None,
    }
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
        Lang::Rust => {
            edited.replace_range(
                body.start_byte()..body.end_byte(),
                &format!("{{\n{new_body}\n}}"),
            );
        }
        Lang::Python => {
            // The body node starts at the first statement (after the indent);
            // replacing without a leading newline keeps exactly one indent.
            edited.replace_range(body.start_byte()..body.end_byte(), new_body.trim_end());
        }
        _ => bail!("unsupported"),
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
    if matches!(node.kind(), "identifier" | "type_identifier")
        && node.utf8_text(source.as_bytes()).unwrap_or("") == name
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

fn move_function(src: &Path, dest: &Path, lang: Lang, name: &str) -> anyhow::Result<EditResult> {
    let source = std::fs::read_to_string(src)?;
    let tree = parse(lang, &source)?;
    let func = find_function(&tree, &source, lang, name)
        .with_context(|| format!("function '{name}' not found in {}", src.display()))?;
    let text = func.utf8_text(source.as_bytes()).unwrap_or("").to_string();

    // Write the destination FIRST: if it fails, the source is untouched.
    let mut dest_content = if dest.exists() {
        std::fs::read_to_string(dest)?
    } else {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)?;
        }
        String::new()
    };
    if !dest_content.is_empty() && !dest_content.ends_with('\n') {
        dest_content.push('\n');
    }
    dest_content.push('\n');
    dest_content.push_str(text.trim_end());
    dest_content.push('\n');
    std::fs::write(dest, &dest_content)?;
    reparse_check(dest, lang)?;

    let mut remaining = source.clone();
    let end = if source.as_bytes().get(func.end_byte()) == Some(&b'\n') {
        func.end_byte() + 1
    } else {
        func.end_byte()
    };
    remaining.replace_range(func.start_byte()..end, "");
    std::fs::write(src, &remaining)?;

    reparse_check(src, lang)?;

    Ok(EditResult {
        message: format!("Moved {name} from {} to {}", src.display(), dest.display()),
        first_line: func.start_position().row + 1,
    })
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
}
