//! Outbound file attachments for the gateway (Telegram sendDocument, etc.).
//!
//! The agent uses the `attach_file` tool to stage a file under the session
//! outbound dir; the gateway then delivers it after the text reply.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::{ContentBlock, Message};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

/// Max single attachment size (Telegram bot upload practical cap we enforce).
pub const MAX_ATTACH_BYTES: usize = 20 * 1024 * 1024;

#[derive(Debug, Clone, Default)]
pub struct AttachmentLog {
    inner: Arc<Mutex<Vec<PathBuf>>>,
}

impl AttachmentLog {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Vec::new())),
        }
    }

    pub fn push(&self, path: PathBuf) {
        if let Ok(mut g) = self.inner.lock() {
            if !g.iter().any(|p| p == &path) {
                g.push(path);
            }
        }
    }

    pub fn take(&self) -> Vec<PathBuf> {
        self.inner
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default()
    }

    pub fn clone_handle(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

#[derive(Deserialize, JsonSchema)]
struct AttachArgs {
    /// File name as the user should receive it (e.g. hello.py). No directories.
    filename: String,
    /// Full file contents to send.
    content: String,
    /// Optional short caption shown with the attachment.
    #[serde(default)]
    caption: Option<String>,
}

/// Stages a file for gateway delivery via Telegram sendDocument (etc.).
pub struct AttachFileTool {
    out_dir: PathBuf,
    log: AttachmentLog,
}

impl AttachFileTool {
    pub fn new(out_dir: PathBuf, log: AttachmentLog) -> Self {
        Self { out_dir, log }
    }
}

#[async_trait]
impl AgentTool for AttachFileTool {
    fn name(&self) -> &str {
        "attach_file"
    }

    fn description(&self) -> &str {
        "Attach a file to your reply so the user receives it as a downloadable document \
         (Telegram/document channels). Use this whenever the user asks you to send, \
         share, or create a file for them — do not only paste code in text."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(AttachArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("attach_file: send a downloadable file attachment to the user")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: AttachArgs = serde_json::from_value(ctx.args)?;
        if args.content.len() > MAX_ATTACH_BYTES {
            anyhow::bail!(
                "content too large ({} bytes, max {MAX_ATTACH_BYTES})",
                args.content.len()
            );
        }
        let name = sanitize_filename(&args.filename);
        if name.is_empty() {
            anyhow::bail!("invalid filename");
        }
        std::fs::create_dir_all(&self.out_dir)?;
        let path = self.out_dir.join(&name);
        std::fs::write(&path, args.content.as_bytes())?;
        self.log.push(path.clone());
        let cap = args
            .caption
            .as_deref()
            .map(|c| format!(" caption={c:?}"))
            .unwrap_or_default();
        Ok(ToolOutput::text(format!(
            "Queued attachment {} ({} bytes){cap} — will be sent with your reply.",
            name,
            args.content.len()
        )))
    }
}

pub fn sanitize_filename(name: &str) -> String {
    let base = Path::new(name)
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or(name);
    let cleaned: String = base
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+') {
                c
            } else {
                '_'
            }
        })
        .collect();
    let cleaned = cleaned.trim_matches('.').to_string();
    if cleaned.is_empty() || cleaned == "_" {
        "file.bin".into()
    } else {
        cleaned.chars().take(180).collect()
    }
}

/// Paths written by `write` tool during the turn (gateway-code mode).
pub fn paths_from_write_results(msgs: &[Message]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for m in msgs {
        let Message::ToolResult(tr) = m else { continue };
        if tr.tool_name != "write" {
            continue;
        }
        let text: String = tr
            .content
            .iter()
            .filter_map(|b| match b {
                ContentBlock::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n");
        // "Successfully wrote N bytes to /abs/path"
        if let Some(idx) = text.find(" bytes to ") {
            let path = text[idx + " bytes to ".len()..].trim();
            if !path.is_empty() {
                let p = PathBuf::from(path);
                if p.is_file() {
                    out.push(p);
                }
            }
        }
    }
    out
}

/// If the model only pasted a fenced code block and named a file, materialize it.
/// Patterns: ```python\n# file: hello.py  or  ```hello.py
pub fn materialize_fenced_files(reply: &str, out_dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut rest = reply;
    while let Some(start) = rest.find("```") {
        rest = &rest[start + 3..];
        let (header, after_header) = match rest.find('\n') {
            Some(i) => (&rest[..i], &rest[i + 1..]),
            None => break,
        };
        let Some(end) = after_header.find("```") else {
            break;
        };
        let body = &after_header[..end];
        rest = &after_header[end + 3..];

        let header = header.trim();
        let mut filename: Option<String> = None;
        // ```hello.py or ```python hello.py
        for part in header.split_whitespace() {
            if part.contains('.')
                && part.len() < 120
                && !part.starts_with("http")
                && part.chars().all(|c| {
                    c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+')
                })
            {
                filename = Some(part.to_string());
                break;
            }
        }
        // first line comment: # file: name / // file: name
        if filename.is_none() {
            if let Some(first) = body.lines().next() {
                let f = first.trim();
                for prefix in ["# file:", "#file:", "// file:", "//file:", "/* file:"] {
                    if let Some(rest) = f
                        .to_ascii_lowercase()
                        .strip_prefix(&prefix.to_ascii_lowercase())
                        .or_else(|| {
                            // case-sensitive variants already lowercased line
                            None
                        })
                    {
                        let _ = rest;
                    }
                }
                let lower = f.to_ascii_lowercase();
                for prefix in ["# file:", "#file:", "// file:", "//file:"] {
                    if let Some(idx) = lower.find(prefix) {
                        let name = f[idx + prefix.len()..].trim().trim_matches('*').trim();
                        if !name.is_empty() {
                            filename = Some(sanitize_filename(name));
                        }
                        break;
                    }
                }
            }
        }
        let Some(name) = filename else {
            continue;
        };
        let name = sanitize_filename(&name);
        if body.trim().is_empty() || body.len() > MAX_ATTACH_BYTES {
            continue;
        }
        let _ = std::fs::create_dir_all(out_dir);
        let path = out_dir.join(&name);
        if std::fs::write(&path, body).is_ok() {
            out.push(path);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitize_strips_paths() {
        assert_eq!(sanitize_filename("../../etc/passwd"), "passwd");
        assert_eq!(sanitize_filename("hello.py"), "hello.py");
    }

    #[test]
    fn fence_materialize_named_lang_file() {
        let dir = tempfile::tempdir().unwrap();
        let reply = "Here:\n```hello.py\nprint('hi')\n```\n";
        let paths = materialize_fenced_files(reply, dir.path());
        assert_eq!(paths.len(), 1);
        assert_eq!(
            std::fs::read_to_string(&paths[0]).unwrap().trim(),
            "print('hi')"
        );
    }

    #[tokio::test]
    async fn attach_tool_queues_file() {
        let dir = tempfile::tempdir().unwrap();
        let log = AttachmentLog::new();
        let tool = AttachFileTool::new(dir.path().to_path_buf(), log.clone_handle());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({
                    "filename": "hello.py",
                    "content": "print(1)\n"
                }),
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        assert!(out.content[0].as_text().unwrap().contains("Queued"));
        let taken = log.take();
        assert_eq!(taken.len(), 1);
        assert_eq!(std::fs::read_to_string(&taken[0]).unwrap(), "print(1)\n");
    }
}
