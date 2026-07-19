use std::path::PathBuf;

use anyhow::{bail, Context as _};
use async_trait::async_trait;
use base64::Engine;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::paths;
use crate::truncate::{self, MAX_LINES};

static READ_CACHE: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<PathBuf, (std::time::SystemTime, String)>>,
> = std::sync::OnceLock::new();

fn cache(
) -> &'static std::sync::Mutex<std::collections::HashMap<PathBuf, (std::time::SystemTime, String)>>
{
    READ_CACHE.get_or_init(|| std::sync::Mutex::new(std::collections::HashMap::new()))
}

/// Cap on bytes loaded per file. A read tool paged by line offset must never
/// pull a multi-gigabyte file fully into memory (and into the 500-entry cache);
/// beyond this the content is truncated and the caller flags it.
pub const MAX_READ_BYTES: u64 = 20 * 1024 * 1024;

fn read_cached(path: &std::path::Path) -> anyhow::Result<String> {
    let mtime = std::fs::metadata(path).and_then(|m| m.modified()).ok();
    if let Some(mtime) = mtime {
        let map = cache().lock().unwrap();
        if let Some((cached_mtime, content)) = map.get(path) {
            if *cached_mtime == mtime {
                return Ok(content.clone());
            }
        }
    }
    use std::io::Read as _;
    let file =
        std::fs::File::open(path).with_context(|| format!("failed to read {}", path.display()))?;
    let mut raw = Vec::new();
    // take() bounds the read so a huge or growing file can't exhaust memory.
    file.take(MAX_READ_BYTES)
        .read_to_end(&mut raw)
        .with_context(|| format!("failed to read {}", path.display()))?;
    let content = String::from_utf8_lossy(&raw).into_owned();
    if let Some(mtime) = mtime {
        let mut map = cache().lock().unwrap();
        if map.len() > 500 {
            let drop: Vec<PathBuf> = map.keys().take(100).cloned().collect();
            for k in drop {
                map.remove(&k);
            }
        }
        map.insert(path.to_path_buf(), (mtime, content.clone()));
    }
    Ok(content)
}

#[derive(Deserialize, JsonSchema)]
struct ReadArgs {
    /// Path to the file to read
    path: String,
    /// 1-indexed line number to start reading from
    offset: Option<usize>,
    /// Maximum number of lines to read
    limit: Option<usize>,
}

pub struct ReadTool {
    cwd: PathBuf,
}

impl ReadTool {
    pub fn new(cwd: PathBuf) -> Self {
        ReadTool { cwd }
    }
}

const IMAGE_EXTS: &[(&str, &str)] = &[
    ("jpg", "image/jpeg"),
    ("jpeg", "image/jpeg"),
    ("png", "image/png"),
    ("gif", "image/gif"),
    ("webp", "image/webp"),
    ("bmp", "image/bmp"),
];

#[async_trait]
impl AgentTool for ReadTool {
    fn name(&self) -> &str {
        "read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports text files (with optional offset/limit for paging) and images (returned as image content)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(ReadArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("read: read file contents, optionally paged with offset/limit")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(ctx.args)?;
        let path = paths::resolve_contained(&self.cwd, &args.path)?;

        if let Some(ext) = path
            .extension()
            .and_then(|e| e.to_str())
            .map(|e| e.to_ascii_lowercase())
        {
            if let Some((_, mime)) = IMAGE_EXTS.iter().find(|(e, _)| *e == ext) {
                let size = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
                if size > 20 * 1024 * 1024 {
                    anyhow::bail!(
                        "image too large ({size} bytes, cap is 20MB); downscale it first (e.g. with bash)"
                    );
                }
                let bytes = std::fs::read(&path)
                    .with_context(|| format!("failed to read {}", path.display()))?;
                let data = base64::engine::general_purpose::STANDARD.encode(bytes);
                return Ok(ToolOutput {
                    content: vec![
                        pirs_ai::ContentBlock::text(format!("Read image file {}", path.display())),
                        pirs_ai::ContentBlock::Image {
                            data,
                            mime_type: mime.to_string(),
                        },
                    ],
                    details: None,
                    terminate: false,
                });
            }
        }

        let byte_capped = std::fs::metadata(&path).map(|m| m.len()).unwrap_or(0) > MAX_READ_BYTES;
        let content = std::borrow::Cow::from(read_cached(&path)?);
        let offset = args.offset.unwrap_or(1);
        let total_lines = content.lines().count();
        if offset > total_lines && total_lines > 0 {
            bail!(
                "offset {offset} is past end of file ({} has {total_lines} lines)",
                path.display()
            );
        }
        let limit = args.limit.unwrap_or(MAX_LINES);
        let window = truncate::head(&content, offset, limit);

        let mut text = window.text;
        if text.lines().count() == 1 && text.len() > 2000 {
            text = truncate::truncate_line(&text, 2000);
            text.push_str("\n[Line too long. Use bash: sed -n 'Np' <path> | head -c 2000]");
        } else if window.truncated {
            text.push_str(&format!(
                "\n[Showing lines {}-{} of {}. Use offset={} to continue.]",
                window.start_line,
                window.end_line,
                window.total_lines,
                window.end_line + 1
            ));
        }
        if byte_capped {
            text.push_str(&format!(
                "\n[File exceeds {} MB; only the first {} MB were read. Use bash (e.g. tail/sed) for the rest.]",
                MAX_READ_BYTES / (1024 * 1024),
                MAX_READ_BYTES / (1024 * 1024)
            ));
        }

        Ok(ToolOutput::text(text))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    async fn run(tool: &ReadTool, args: Value) -> anyhow::Result<ToolOutput> {
        tool.execute(ToolExecContext {
            tool_call_id: "t".into(),
            args,
            cancel: CancellationToken::new(),
            on_update: None,
        })
        .await
    }

    #[tokio::test]
    async fn reads_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("f.txt");
        std::fs::write(&file, "l1\nl2\nl3\nl4\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = run(
            &tool,
            serde_json::json!({"path": "f.txt", "offset": 2, "limit": 2}),
        )
        .await
        .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.starts_with("l2\nl3"));
        assert!(text.contains("offset=4"));
    }

    #[tokio::test]
    async fn oversized_file_is_byte_capped() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("big.txt");
        // Just over the cap so only a prefix is read.
        let line_count = (MAX_READ_BYTES as usize / 3) + 1000;
        std::fs::write(&file, "ab\n".repeat(line_count)).unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"path": "big.txt"}))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(
            text.contains("exceeds"),
            "expected byte-cap notice: {}",
            &text[text.len().saturating_sub(200)..]
        );
    }

    #[tokio::test]
    async fn offset_past_eof_errors() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("f.txt"), "l1\n").unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let err = run(&tool, serde_json::json!({"path": "f.txt", "offset": 99}))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("past end of file"));
    }

    #[tokio::test]
    async fn reads_png_as_image() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("i.png"), [0x89, 0x50, 0x4e, 0x47]).unwrap();
        let tool = ReadTool::new(dir.path().to_path_buf());
        let out = run(&tool, serde_json::json!({"path": "i.png"}))
            .await
            .unwrap();
        assert!(out
            .content
            .iter()
            .any(|b| matches!(b, pirs_ai::ContentBlock::Image { .. })));
    }
}
