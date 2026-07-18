use std::collections::HashMap;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use anyhow::{bail, Context as _};
use serde_json::{json, Value};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::oneshot;

type PendingMap = Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, String>>>>>;

pub struct LspClient {
    stdin: std::sync::Arc<tokio::sync::Mutex<tokio::process::ChildStdin>>,
    pending: PendingMap,
    next_id: AtomicU64,
    opened: Mutex<HashMap<String, u64>>,
    child: tokio::sync::Mutex<Child>,
}

pub struct ServerSpec {
    pub language: &'static str,
    pub command: &'static str,
    pub args: &'static [&'static str],
    pub extensions: &'static [&'static str],
}

pub const SERVERS: &[ServerSpec] = &[
    ServerSpec {
        language: "rust",
        command: "rust-analyzer",
        args: &[],
        extensions: &["rs"],
    },
    ServerSpec {
        language: "typescript",
        command: "typescript-language-server",
        args: &["--stdio"],
        extensions: &["ts", "tsx", "js", "jsx"],
    },
    ServerSpec {
        language: "python",
        command: "pyright-langserver",
        args: &["--stdio"],
        extensions: &["py"],
    },
    ServerSpec {
        language: "go",
        command: "gopls",
        args: &[],
        extensions: &["go"],
    },
];

pub fn server_for_file(path: &std::path::Path) -> Option<&'static ServerSpec> {
    let ext = path.extension().and_then(|e| e.to_str())?;
    SERVERS.iter().find(|s| s.extensions.contains(&ext))
}

pub fn server_available(spec: &ServerSpec) -> bool {
    std::process::Command::new(spec.command)
        .arg("--version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

impl LspClient {
    pub async fn spawn(
        command: &str,
        args: &[&str],
        root: &std::path::Path,
    ) -> anyhow::Result<Arc<Self>> {
        let mut child = Command::new(command)
            .args(args)
            .current_dir(root)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("failed to spawn LSP server '{command}'"))?;

        let stdin = child.stdin.take().context("no stdin on LSP server")?;
        let stdin = std::sync::Arc::new(tokio::sync::Mutex::new(stdin));
        let stdout = child.stdout.take().context("no stdout on LSP server")?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        {
            let pending = Arc::clone(&pending);
            let stdin_writer = std::sync::Arc::clone(&stdin);
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout);
                loop {
                    match read_message(&mut reader).await {
                        Ok(Some(value)) => {
                            if let Some(id) = value.get("id").and_then(|i| i.as_u64()) {
                                let tx = pending.lock().unwrap().remove(&id);
                                if let Some(tx) = tx {
                                    if let Some(err) = value.get("error") {
                                        let _ = tx.send(Err(err.to_string()));
                                    } else {
                                        let _ = tx.send(Ok(value
                                            .get("result")
                                            .cloned()
                                            .unwrap_or(Value::Null)));
                                    }
                                }
                            }
                        }
                        Ok(None) => break,
                        Err(_) => {
                            let mut all = pending.lock().unwrap();
                            for (_, tx) in all.drain() {
                                let _ = tx.send(Err("LSP read error".to_string()));
                            }
                            break;
                        }
                    }
                }
            });
        }

        let client = Arc::new(LspClient {
            stdin: std::sync::Arc::clone(&stdin),
            pending,
            next_id: AtomicU64::new(1),
            opened: Mutex::new(HashMap::new()),
            child: tokio::sync::Mutex::new(child),
        });

        client.initialize(root).await?;
        Ok(client)
    }

    async fn write_message(&self, value: &Value) -> anyhow::Result<()> {
        let body = serde_json::to_string(value)?;
        let mut stdin = self.stdin.lock().await;
        stdin
            .write_all(format!("Content-Length: {}\r\n\r\n", body.len()).as_bytes())
            .await?;
        stdin.write_all(body.as_bytes()).await?;
        stdin.flush().await?;
        Ok(())
    }

    async fn request(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": method,
            "params": params,
        }))
        .await?;
        match tokio::time::timeout(Duration::from_secs(30), rx).await {
            Ok(Ok(Ok(v))) => Ok(v),
            Ok(Ok(Err(e))) => bail!("{e}"),
            Ok(Err(_)) => bail!("LSP server dropped the request"),
            Err(_) => bail!("LSP request '{method}' timed out"),
        }
    }

    async fn notify(&self, method: &str, params: Value) -> anyhow::Result<()> {
        self.write_message(&json!({
            "jsonrpc": "2.0",
            "method": method,
            "params": params,
        }))
        .await
    }

    async fn initialize(&self, root: &std::path::Path) -> anyhow::Result<()> {
        let root_uri = format!("file://{}", root.canonicalize()?.display());
        self.request(
            "initialize",
            json!({
                "processId": std::process::id(),
                "rootUri": root_uri,
                "capabilities": {
                    "textDocument": {
                        "definition": { "linkSupport": false },
                        "references": {},
                        "hover": { "contentFormat": ["plaintext"] },
                        "documentSymbol": { "hierarchicalDocumentSymbolSupport": true }
                    }
                },
                "clientInfo": { "name": "pirs", "version": env!("CARGO_PKG_VERSION") },
            }),
        )
        .await
        .context("LSP initialize failed")?;
        self.notify("initialized", json!({})).await?;
        Ok(())
    }

    pub async fn open_document(
        &self,
        path: &std::path::Path,
        language: &str,
    ) -> anyhow::Result<()> {
        let uri = uri_for(path);
        {
            let opened = self.opened.lock().unwrap();
            if opened.contains_key(&uri) {
                return Ok(());
            }
        }
        let text = std::fs::read_to_string(path)?;
        self.notify(
            "textDocument/didOpen",
            json!({
                "textDocument": {
                    "uri": uri,
                    "languageId": language,
                    "version": 1,
                    "text": text,
                }
            }),
        )
        .await?;
        self.opened.lock().unwrap().insert(uri, 1);
        Ok(())
    }

    pub async fn touch_document(
        &self,
        path: &std::path::Path,
        language: &str,
    ) -> anyhow::Result<()> {
        let uri = uri_for(path);
        let version = {
            let mut opened = self.opened.lock().unwrap();
            match opened.get_mut(&uri) {
                Some(v) => {
                    *v += 1;
                    *v
                }
                None => 0,
            }
        };
        if version == 0 {
            return self.open_document(path, language).await;
        }
        let text = std::fs::read_to_string(path)?;
        self.notify(
            "textDocument/didChange",
            json!({
                "textDocument": { "uri": uri, "version": version },
                "contentChanges": [{ "text": text }]
            }),
        )
        .await
    }

    async fn request_on_document(
        &self,
        method: &str,
        path: &std::path::Path,
        params: Value,
        language: &str,
    ) -> anyhow::Result<Value> {
        match self.request(method, params.clone()).await {
            Ok(v) => Ok(v),
            Err(e)
                if e.to_string().contains("-32801")
                    || e.to_string().contains("content modified") =>
            {
                self.touch_document(path, language).await?;
                self.request(method, params).await
            }
            Err(e) => Err(e),
        }
    }

    pub async fn definition_in(
        &self,
        method: &str,
        path: &std::path::Path,
        params: Value,
        language: &str,
    ) -> anyhow::Result<Value> {
        self.request_on_document(method, path, params, language)
            .await
    }

    fn position_params(&self, path: &std::path::Path, line: u32, character: u32) -> Value {
        json!({
            "textDocument": { "uri": uri_for(path) },
            "position": { "line": line.saturating_sub(1), "character": character.saturating_sub(1) }
        })
    }

    pub async fn definition(
        &self,
        path: &std::path::Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Value> {
        self.request_indexed(
            "textDocument/definition",
            self.position_params(path, line, character),
        )
        .await
    }

    pub async fn references(
        &self,
        path: &std::path::Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Value> {
        let mut params = self.position_params(path, line, character);
        params["context"] = json!({ "includeDeclaration": true });
        self.request_indexed("textDocument/references", params)
            .await
    }

    /// rust-analyzer (and others) answer with empty results while indexing;
    /// retry briefly instead of returning a wrong "no results".
    async fn request_indexed(&self, method: &str, params: Value) -> anyhow::Result<Value> {
        for attempt in 0..6 {
            let result = self.request(method, params.clone()).await;
            match result {
                Ok(v) => {
                    let empty = match &v {
                        Value::Array(a) => a.is_empty(),
                        Value::Null => true,
                        _ => false,
                    };
                    if !empty {
                        return Ok(v);
                    }
                }
                Err(e) => {
                    let msg = e.to_string();
                    if msg.contains("-32801") || msg.contains("content modified") {
                        if let Some(uri) =
                            params.pointer("/textDocument/uri").and_then(|u| u.as_str())
                        {
                            let path = std::path::PathBuf::from(uri.trim_start_matches("file://"));
                            let lang = crate::client::server_for_file(&path)
                                .map(|s| s.language)
                                .unwrap_or("plaintext");
                            let _ = self.touch_document(&path, lang).await;
                        }
                    } else {
                        return Err(e);
                    }
                }
            }
            if attempt < 5 {
                tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            }
        }
        self.request(method, params).await
    }

    pub async fn hover(
        &self,
        path: &std::path::Path,
        line: u32,
        character: u32,
    ) -> anyhow::Result<Value> {
        self.request(
            "textDocument/hover",
            self.position_params(path, line, character),
        )
        .await
    }

    pub async fn document_symbols(&self, path: &std::path::Path) -> anyhow::Result<Value> {
        self.request(
            "textDocument/documentSymbol",
            json!({ "textDocument": { "uri": uri_for(path) } }),
        )
        .await
    }

    pub async fn shutdown(&self) {
        let _ = self.request("shutdown", json!({})).await;
        let _ = self.notify("exit", json!({})).await;
        let mut child = self.child.lock().await;
        let _ = child.kill().await;
    }
}

fn uri_for(path: &std::path::Path) -> String {
    let abs = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    format!("file://{}", abs.display())
}

async fn read_message(
    reader: &mut BufReader<tokio::process::ChildStdout>,
) -> anyhow::Result<Option<Value>> {
    let mut content_length = 0usize;
    loop {
        let mut line = String::new();
        let n = reader.read_line(&mut line).await?;
        if n == 0 {
            return Ok(None);
        }
        let line = line.trim();
        if line.is_empty() {
            break;
        }
        if let Some(rest) = line.to_ascii_lowercase().strip_prefix("content-length:") {
            content_length = rest.trim().parse().unwrap_or(0);
        }
    }
    if content_length == 0 {
        bail!("missing Content-Length");
    }
    let mut body = vec![0u8; content_length];
    reader.read_exact(&mut body).await?;
    let value: Value = serde_json::from_slice(&body)?;
    Ok(Some(value))
}

pub fn format_location(loc: &Value, root: &std::path::Path) -> Option<String> {
    let uri = loc.get("uri").and_then(|u| u.as_str())?;
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    let rel = std::path::Path::new(path)
        .strip_prefix(root)
        .unwrap_or(std::path::Path::new(path))
        .to_string_lossy()
        .to_string();
    let line = loc
        .pointer("/range/start/line")
        .and_then(|l| l.as_u64())
        .unwrap_or(0)
        + 1;
    Some(format!("{rel}:{line}"))
}
