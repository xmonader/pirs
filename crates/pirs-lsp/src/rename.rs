//! `rename_symbol` — a compound tool that renames a symbol *everywhere* in one
//! call.
//!
//! Without it, renaming means: find every reference (easy to miss one), then edit
//! each site by hand across every file, keeping offsets straight. This tool asks
//! the language server for the full set of edits (a `WorkspaceEdit`) and applies
//! them atomically, so a rename is one tool call and is as correct as the server's
//! reference analysis — not a fragile text search-and-replace.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};

use crate::client::{server_for_file, LspClient};
use crate::edit::apply_workspace_edit_contained;

#[derive(Deserialize, JsonSchema)]
struct RenameArgs {
    /// File path (relative to the workspace) containing the symbol.
    path: String,
    /// 1-based line of the symbol.
    line: u32,
    /// 1-based column of the symbol.
    character: u32,
    /// The new name for the symbol.
    new_name: String,
}

pub struct RenameSymbolTool {
    root: PathBuf,
    clients: tokio::sync::Mutex<std::collections::HashMap<String, Arc<LspClient>>>,
}

impl RenameSymbolTool {
    pub fn new(root: PathBuf) -> Self {
        RenameSymbolTool {
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
impl AgentTool for RenameSymbolTool {
    fn name(&self) -> &str {
        "rename_symbol"
    }

    fn description(&self) -> &str {
        "Rename a symbol (variable, function, type, ...) across the whole project \
         in one step, via the language server. Give the file, 1-based line/column \
         of the symbol, and the new name; every reference is updated consistently. \
         Prefer this over manual edit-per-site for renames (rust/typescript/python/go)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RenameArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("rename_symbol: project-wide symbol rename via the language server")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: RenameArgs = serde_json::from_value(ctx.args.clone())?;
        if args.new_name.trim().is_empty() {
            anyhow::bail!("new_name must not be empty");
        }
        // Same workspace confinement as other file tools (review C-7).
        let path = pirs_tools::paths::resolve_contained(&self.root, &args.path)?;
        if !path.exists() {
            anyhow::bail!("file not found: {}", path.display());
        }
        let spec = server_for_file(&path)
            .ok_or_else(|| anyhow::anyhow!("no LSP server for {}", path.display()))?;
        let client = self.client_for(&path).await?;
        client.open_document(&path, spec.language).await?;

        ctx.emit_update(format!("renaming to {}", args.new_name));
        let workspace_edit = client
            .rename(&path, args.line, args.character, &args.new_name)
            .await?;
        if workspace_edit.is_null() {
            anyhow::bail!(
                "the server declined to rename the symbol at {}:{}:{} \
                 (not a renameable symbol, or it could not be resolved)",
                args.path,
                args.line,
                args.character
            );
        }

        // Apply only edits under work roots (LSP may return absolute URIs outside).
        let changed = apply_workspace_edit_contained(&self.root, &workspace_edit)?;
        let total: usize = changed.iter().map(|(_, n)| n).sum();
        let mut lines = vec![format!(
            "renamed to `{}`: {total} edit(s) across {} file(s)",
            args.new_name,
            changed.len()
        )];
        for (p, n) in &changed {
            let rel = p.strip_prefix(&self.root).unwrap_or(p);
            lines.push(format!("  {} ({n} edit(s))", rel.display()));
        }
        Ok(
            ToolOutput::text(lines.join("\n")).with_details(serde_json::json!({
                "new_name": args.new_name,
                "files_changed": changed.len(),
                "total_edits": total,
            })),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_new_name_is_rejected_before_any_lsp_work() {
        // A blank new_name must fail fast (no server spawn). We can't drive a real
        // rename in a unit test (needs a language server), so guard the cheap
        // validation path here; the edit-application logic is covered in `edit`.
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("a.rs"), "fn foo() {}\n").unwrap();
        let tool = RenameSymbolTool::new(dir.path().to_path_buf());
        let ctx = ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({
                "path": "a.rs", "line": 1, "character": 4, "new_name": "  "
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        };
        let err = rt.block_on(tool.execute(ctx)).unwrap_err().to_string();
        assert!(err.contains("new_name must not be empty"), "{err}");
    }

    #[test]
    fn missing_file_is_rejected() {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let tool = RenameSymbolTool::new(dir.path().to_path_buf());
        let ctx = ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({
                "path": "nope.rs", "line": 1, "character": 1, "new_name": "bar"
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        };
        let err = rt.block_on(tool.execute(ctx)).unwrap_err().to_string();
        assert!(err.contains("file not found"), "{err}");
    }
}
