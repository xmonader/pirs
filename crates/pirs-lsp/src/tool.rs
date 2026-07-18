use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Context as _;
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::{format_location, server_for_file, LspClient};

#[derive(Deserialize, JsonSchema)]
struct LspArgs {
    /// Action: definition | references | hover | symbols
    action: String,
    /// File path (relative to workspace)
    path: String,
    /// 1-based line of the symbol position (not needed for symbols)
    line: Option<u32>,
    /// 1-based column of the symbol position (not needed for symbols)
    character: Option<u32>,
}

pub struct LspTool {
    root: PathBuf,
    clients: tokio::sync::Mutex<std::collections::HashMap<String, Arc<LspClient>>>,
}

impl LspTool {
    pub fn new(root: PathBuf) -> Self {
        LspTool {
            root,
            clients: tokio::sync::Mutex::new(std::collections::HashMap::new()),
        }
    }

    async fn client_for(&self, path: &Path) -> anyhow::Result<Arc<LspClient>> {
        let spec = server_for_file(path)
            .ok_or_else(|| anyhow::anyhow!("no LSP server registered for {}", path.display()))?;
        let mut clients = self.clients.lock().await;
        if let Some(client) = clients.get(spec.language) {
            return Ok(Arc::clone(client));
        }
        let client = LspClient::spawn(spec.command, spec.args, &self.root).await?;
        clients.insert(spec.language.to_string(), Arc::clone(&client));
        Ok(client)
    }

    pub async fn shutdown_all(&self) {
        let clients = self.clients.lock().await;
        for client in clients.values() {
            client.shutdown().await;
        }
    }
}

#[async_trait]
impl AgentTool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Precise language-server queries: jump to definition, find all references, hover type info, or list a file's symbols. Use for exact answers where code_map is approximate (rust/typescript/python/go)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(LspArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("lsp: precise definition/references/hover/symbols via language servers")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: LspArgs = serde_json::from_value(ctx.args)?;
        let path = self.root.join(&args.path);
        if !path.exists() {
            anyhow::bail!("file not found: {}", path.display());
        }
        let client = self.client_for(&path).await?;
        let spec = server_for_file(&path).unwrap();

        match args.action.as_str() {
            "symbols" => {
                client.open_document(&path, spec.language).await?;
                let result = client.document_symbols(&path).await?;
                Ok(ToolOutput::text(format_symbols(&result)))
            }
            action => {
                let line = args.line.context("line required")?;
                let character = args.character.unwrap_or(1);
                client.open_document(&path, spec.language).await?;
                let result = match action {
                    "definition" => client.definition(&path, line, character).await?,
                    "references" => client.references(&path, line, character).await?,
                    "hover" => client.hover(&path, line, character).await?,
                    other => anyhow::bail!("unknown action '{other}'"),
                };
                let text = match action {
                    "hover" => format_hover(&result),
                    _ => format_locations(&result, &self.root),
                };
                Ok(ToolOutput::text(text))
            }
        }
    }
}

fn format_locations(result: &Value, root: &Path) -> String {
    let mut locs: Vec<String> = Vec::new();
    match result {
        Value::Array(arr) => {
            for loc in arr {
                if let Some(f) = format_location(loc, root) {
                    locs.push(f);
                }
            }
        }
        Value::Object(_) => {
            if let Some(f) = format_location(result, root) {
                locs.push(f);
            }
            if let Some(arr) = result.as_array() {
                for loc in arr {
                    if let Some(f) = format_location(loc, root) {
                        locs.push(f);
                    }
                }
            }
        }
        _ => {}
    }
    if locs.is_empty() {
        "no locations found".to_string()
    } else {
        locs.join("\n")
    }
}

fn format_hover(result: &Value) -> String {
    let contents = result.get("contents").cloned().unwrap_or(Value::Null);
    match contents {
        Value::String(s) => s,
        Value::Object(o) => o
            .get("value")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string(),
        Value::Array(arr) => arr
            .iter()
            .filter_map(|c| match c {
                Value::String(s) => Some(s.clone()),
                Value::Object(o) => o
                    .get("value")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"),
        _ => "no hover info".to_string(),
    }
}

fn format_symbols(result: &Value) -> String {
    fn walk(symbols: &[Value], depth: usize, out: &mut Vec<String>) {
        for sym in symbols {
            let name = sym.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let kind = sym.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
            let line = sym
                .pointer("/selectionRange/start/line")
                .or_else(|| sym.pointer("/range/start/line"))
                .and_then(|l| l.as_u64())
                .unwrap_or(0)
                + 1;
            let kind_name = match kind {
                5 => "class",
                6 => "method",
                9 => "struct",
                10 => "enum",
                12 => "fn",
                23 => "trait",
                _ => "sym",
            };
            out.push(format!(
                "{}{} {} (:{line})",
                "  ".repeat(depth),
                kind_name,
                name
            ));
            if let Some(children) = sym.get("children").and_then(|c| c.as_array()) {
                walk(children, depth + 1, out);
            }
        }
    }
    let mut out = Vec::new();
    if let Some(arr) = result.as_array() {
        walk(arr, 0, &mut out);
    }
    if out.is_empty() {
        "no symbols".to_string()
    } else {
        out.join("\n")
    }
}
