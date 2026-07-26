use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::client::{format_location, path_from_uri, server_for_file, LspClient};

#[derive(Deserialize, JsonSchema)]
struct LspArgs {
    /// Action: definition | references | hover | symbols | diagnostics |
    /// workspace_symbols | implementations | type_definition |
    /// incoming_calls | outgoing_calls | find_symbol
    action: String,
    /// File path (relative to workspace); optional for workspace_symbols
    path: Option<String>,
    /// 1-based line of the symbol (not needed for symbols/diagnostics/workspace_symbols)
    line: Option<u32>,
    /// 1-based column of the symbol
    character: Option<u32>,
    /// Symbol name: for workspace_symbols query, or find_symbol (resolves position)
    name: Option<String>,
    /// Query string for workspace_symbols (alias of name)
    query: Option<String>,
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

    /// Prefer rust-analyzer if present for workspace-wide queries without a path.
    async fn default_client(&self) -> anyhow::Result<Arc<LspClient>> {
        // Prefer an already-spawned client
        {
            let clients = self.clients.lock().await;
            if let Some(c) = clients.values().next() {
                return Ok(Arc::clone(c));
            }
        }
        // Spawn rust-analyzer against root if available
        for spec in crate::client::SERVERS {
            if crate::client::server_available(spec) {
                let client = LspClient::spawn(spec.command, spec.args, &self.root).await?;
                let mut clients = self.clients.lock().await;
                clients.insert(spec.language.to_string(), Arc::clone(&client));
                return Ok(client);
            }
        }
        anyhow::bail!(
            "no LSP server available on PATH (need rust-analyzer, typescript-language-server, pyright-langserver, or gopls)"
        )
    }

    pub async fn shutdown_all(&self) {
        let clients = self.clients.lock().await;
        for client in clients.values() {
            client.shutdown().await;
        }
    }

    async fn open_and_pos(
        &self,
        path: &Path,
        line: Option<u32>,
        character: Option<u32>,
        name: Option<&str>,
    ) -> anyhow::Result<(Arc<LspClient>, u32, u32)> {
        let client = self.client_for(path).await?;
        let spec = server_for_file(path).unwrap();
        client.open_document(path, spec.language).await?;
        let (line, character) = if let (Some(l), c) = (line, character.unwrap_or(1)) {
            (l, c)
        } else if let Some(n) = name {
            client.find_symbol_position(path, n).await?
        } else {
            anyhow::bail!("provide line+character, or name= to resolve position from symbols");
        };
        Ok((client, line, character))
    }
}

#[async_trait]
impl AgentTool for LspTool {
    fn name(&self) -> &str {
        "lsp"
    }

    fn description(&self) -> &str {
        "Language-server awareness (IDE-grade): definition, references, hover, \
         document symbols, workspace_symbols (find by name), implementations, \
         type_definition, incoming_calls/outgoing_calls (call hierarchy), \
         find_symbol (name→position then definition), diagnostics. \
         Prefer over blind grep for navigation; use rename_symbol for project-wide renames. \
         Servers: rust-analyzer / tsserver / pyright / gopls when on PATH."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(LspArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some(
            "lsp: definition|references|hover|symbols|workspace_symbols|implementations|\
             type_definition|incoming_calls|outgoing_calls|find_symbol|diagnostics",
        )
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: LspArgs = serde_json::from_value(ctx.args)?;
        let name = args.name.as_deref().or(args.query.as_deref());

        match args.action.as_str() {
            "diagnostics" => {
                let path_s = args
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("diagnostics requires path"))?;
                let path = pirs_tools::paths::resolve_contained(&self.root, path_s)?;
                if !path.exists() {
                    anyhow::bail!("file not found: {}", path.display());
                }
                let client = self.client_for(&path).await?;
                let spec = server_for_file(&path).unwrap();
                client.open_document(&path, spec.language).await?;
                let diag = client
                    .wait_for_diagnostics(&path, 1500)
                    .await
                    .unwrap_or(Value::Null);
                Ok(ToolOutput::text(format_diagnostics(&diag, &self.root)))
            }
            "symbols" => {
                let path_s = args
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("path required"))?;
                let path = pirs_tools::paths::resolve_contained(&self.root, path_s)?;
                if !path.exists() {
                    anyhow::bail!("file not found: {}", path.display());
                }
                let client = self.client_for(&path).await?;
                let spec = server_for_file(&path).unwrap();
                client.open_document(&path, spec.language).await?;
                let result = client.document_symbols(&path).await?;
                Ok(ToolOutput::text(format_symbols(&result)))
            }
            "workspace_symbols" => {
                let q = name.ok_or_else(|| {
                    anyhow::anyhow!("workspace_symbols requires name= or query=")
                })?;
                let client = if let Some(path_s) = args.path.as_deref() {
                    let path = pirs_tools::paths::resolve_contained(&self.root, path_s)?;
                    let c = self.client_for(&path).await?;
                    let spec = server_for_file(&path).unwrap();
                    let _ = c.open_document(&path, spec.language).await;
                    c
                } else {
                    self.default_client().await?
                };
                let result = client.workspace_symbols(q).await?;
                Ok(ToolOutput::text(format_workspace_symbols(
                    &result,
                    &self.root,
                )))
            }
            "find_symbol" => {
                // Resolve name in file → show definition location(s)
                let path_s = args
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("find_symbol requires path + name"))?;
                let n = name.ok_or_else(|| anyhow::anyhow!("find_symbol requires name="))?;
                let path = pirs_tools::paths::resolve_contained(&self.root, path_s)?;
                if !path.exists() {
                    anyhow::bail!("file not found: {}", path.display());
                }
                let (client, line, character) =
                    self.open_and_pos(&path, None, None, Some(n)).await?;
                let def = client.definition(&path, line, character).await?;
                let mut out = vec![format!(
                    "resolved '{n}' at {}:{}:{}",
                    path_s, line, character
                )];
                let locs = format_locations(&def, &self.root);
                out.push(format!("definition:\n{locs}"));
                // Also list references count/preview
                if let Ok(refs) = client.references(&path, line, character).await {
                    let rlocs = format_locations(&refs, &self.root);
                    out.push(format!("references:\n{rlocs}"));
                }
                Ok(ToolOutput::text(out.join("\n\n")))
            }
            action @ ("definition"
            | "references"
            | "hover"
            | "implementations"
            | "type_definition"
            | "incoming_calls"
            | "outgoing_calls") => {
                let path_s = args
                    .path
                    .as_deref()
                    .ok_or_else(|| anyhow::anyhow!("path required"))?;
                let path = pirs_tools::paths::resolve_contained(&self.root, path_s)?;
                if !path.exists() {
                    anyhow::bail!("file not found: {}", path.display());
                }
                let (client, line, character) = self
                    .open_and_pos(&path, args.line, args.character, name)
                    .await?;
                let text = match action {
                    "definition" => {
                        let r = client.definition(&path, line, character).await?;
                        format_locations(&r, &self.root)
                    }
                    "references" => {
                        let r = client.references(&path, line, character).await?;
                        format_locations(&r, &self.root)
                    }
                    "hover" => {
                        let r = client.hover(&path, line, character).await?;
                        format_hover(&r)
                    }
                    "implementations" => {
                        let r = client.implementations(&path, line, character).await?;
                        format_locations(&r, &self.root)
                    }
                    "type_definition" => {
                        let r = client.type_definition(&path, line, character).await?;
                        format_locations(&r, &self.root)
                    }
                    "incoming_calls" | "outgoing_calls" => {
                        let prep = client
                            .prepare_call_hierarchy(&path, line, character)
                            .await?;
                        let item = first_call_hierarchy_item(&prep).ok_or_else(|| {
                            anyhow::anyhow!(
                                "call hierarchy not available at this position \
                                 (server may not support it, or symbol is not a function)"
                            )
                        })?;
                        let calls = if action == "incoming_calls" {
                            client.incoming_calls(&item).await?
                        } else {
                            client.outgoing_calls(&item).await?
                        };
                        format_call_hierarchy(&calls, action, &self.root)
                    }
                    _ => unreachable!(),
                };
                Ok(ToolOutput::text(text))
            }
            other => anyhow::bail!(
                "unknown action '{other}'. Use: definition|references|hover|symbols|\
                 workspace_symbols|implementations|type_definition|incoming_calls|\
                 outgoing_calls|find_symbol|diagnostics"
            ),
        }
    }
}

fn first_call_hierarchy_item(prep: &Value) -> Option<Value> {
    match prep {
        Value::Array(a) => a.first().cloned(),
        Value::Object(_) => Some(prep.clone()),
        Value::Null => None,
        _ => None,
    }
}

fn format_call_hierarchy(result: &Value, kind: &str, root: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return format!("{kind}: (none or unsupported)");
    };
    if arr.is_empty() {
        return format!("{kind}: (none)");
    }
    let mut lines = vec![format!("{kind} ({}):", arr.len())];
    for item in arr.iter().take(40) {
        let from_to = if kind == "incoming_calls" {
            item.get("from")
        } else {
            item.get("to")
        };
        if let Some(sym) = from_to {
            let name = sym.get("name").and_then(|n| n.as_str()).unwrap_or("?");
            let uri = sym.get("uri").and_then(|u| u.as_str()).unwrap_or("");
            let line = sym
                .pointer("/selectionRange/start/line")
                .or_else(|| sym.pointer("/range/start/line"))
                .and_then(|l| l.as_u64())
                .unwrap_or(0)
                + 1;
            lines.push(format!("  {name}  {}:{line}", uri_to_rel(uri, root)));
        }
    }
    if arr.len() > 40 {
        lines.push(format!("  … +{} more", arr.len() - 40));
    }
    lines.join("\n")
}

fn format_workspace_symbols(result: &Value, root: &Path) -> String {
    let Some(arr) = result.as_array() else {
        return "workspace_symbols: (none)".into();
    };
    if arr.is_empty() {
        return "workspace_symbols: no matches".into();
    }
    let mut lines = vec![format!("workspace_symbols ({} hits):", arr.len())];
    for sym in arr.iter().take(40) {
        let name = sym.get("name").and_then(|n| n.as_str()).unwrap_or("?");
        let kind = sym.get("kind").and_then(|k| k.as_u64()).unwrap_or(0);
        let kind_name = match kind {
            5 => "class",
            6 => "method",
            9 | 23 => "type",
            10 => "enum",
            12 => "fn",
            13 => "var",
            _ => "sym",
        };
        let container = sym
            .get("containerName")
            .and_then(|c| c.as_str())
            .unwrap_or("");
        let loc = sym
            .get("location")
            .and_then(|l| format_location(l, root))
            .unwrap_or_else(|| "?".into());
        if container.is_empty() {
            lines.push(format!("  [{kind_name}] {name}  {loc}"));
        } else {
            lines.push(format!("  [{kind_name}] {container}::{name}  {loc}"));
        }
    }
    if arr.len() > 40 {
        lines.push(format!("  … +{} more", arr.len() - 40));
    }
    lines.join("\n")
}

fn format_diagnostics(params: &Value, root: &Path) -> String {
    let Some(diags) = params.get("diagnostics").and_then(|d| d.as_array()) else {
        return "no diagnostics published yet (server may still be indexing — retry after a moment)"
            .into();
    };
    if diags.is_empty() {
        return "diagnostics: clean (0 issues)".into();
    }
    let uri = params.get("uri").and_then(|u| u.as_str()).unwrap_or("");
    let mut lines = vec![format!(
        "diagnostics for {} ({} issue(s)):",
        uri_to_rel(uri, root),
        diags.len()
    )];
    for d in diags.iter().take(50) {
        let sev = match d.get("severity").and_then(|s| s.as_u64()).unwrap_or(3) {
            1 => "error",
            2 => "warning",
            3 => "info",
            _ => "hint",
        };
        let msg = d
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("")
            .replace('\n', " ");
        let line = d
            .pointer("/range/start/line")
            .and_then(|l| l.as_u64())
            .unwrap_or(0)
            + 1;
        let col = d
            .pointer("/range/start/character")
            .and_then(|c| c.as_u64())
            .unwrap_or(0)
            + 1;
        lines.push(format!("  [{sev}] L{line}:{col} {msg}"));
    }
    if diags.len() > 50 {
        lines.push(format!("  … +{} more", diags.len() - 50));
    }
    lines.join("\n")
}

fn uri_to_rel(uri: &str, root: &Path) -> String {
    let p = path_from_uri(uri);
    p.strip_prefix(root)
        .unwrap_or(&p)
        .to_string_lossy()
        .into()
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

#[cfg(test)]
mod unit_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn find_name_in_symbols_prefers_selection_range() {
        let syms = json!([{
            "name": "outer",
            "kind": 5,
            "selectionRange": {"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}},
            "children": [{
                "name": "target_fn",
                "kind": 12,
                "selectionRange": {"start": {"line": 9, "character": 3}, "end": {"line": 9, "character": 12}},
                "range": {"start": {"line": 9, "character": 0}, "end": {"line": 12, "character": 1}}
            }]
        }]);
        let pos = crate::client::find_name_in_symbols(&syms, "target_fn").unwrap();
        assert_eq!(pos, (10, 4)); // 1-based
        let pos2 = crate::client::find_name_in_symbols(&syms, "Outer::target_fn");
        // bare name match on suffix
        assert!(pos2.is_some());
    }

    #[test]
    fn format_workspace_symbols_renders_hits() {
        let v = json!([{
            "name": "foo",
            "kind": 12,
            "location": {
                "uri": "file:///tmp/proj/src/lib.rs",
                "range": {"start": {"line": 2, "character": 0}, "end": {"line": 2, "character": 3}}
            }
        }]);
        let t = format_workspace_symbols(&v, Path::new("/tmp/proj"));
        assert!(t.contains("foo"));
        assert!(t.contains("fn"));
    }
}
