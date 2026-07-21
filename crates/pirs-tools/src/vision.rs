//! Vision: describe images via OpenAI-compatible multimodal chat.

use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use base64::Engine as _;
use futures::StreamExt;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::{
    non_empty_env, resolve_openai_compat, CompletionOptions, ContentBlock, Context, Message,
    OpenAiCompat, StreamEvent, UserContent, UserMessage,
};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

#[derive(Deserialize, JsonSchema)]
struct VisionArgs {
    /// Path to an image file (png/jpg/webp/gif).
    path: String,
    /// What to look for / question about the image.
    #[serde(default)]
    prompt: Option<String>,
    /// Optional model id (default: env PIRS_VISION_MODEL or qwen-vl / gpt-4o-mini style).
    #[serde(default)]
    model: Option<String>,
}

pub struct VisionDescribeTool {
    cwd: PathBuf,
}

impl VisionDescribeTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for VisionDescribeTool {
    fn name(&self) -> &str {
        "vision_describe"
    }

    fn description(&self) -> &str {
        "Describe or answer questions about an image file using a multimodal OpenAI-compatible model. \
         Needs DASHSCOPE/OPENAI/… key and a vision-capable model."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(VisionArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("vision_describe: analyze an image with a VL model")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: VisionArgs = serde_json::from_value(ctx.args)?;
        let path = crate::paths::resolve_contained(&self.cwd, &args.path)?;
        if !path.is_file() {
            anyhow::bail!("image not found: {}", path.display());
        }
        let bytes = std::fs::read(&path)?;
        if bytes.len() > 12 * 1024 * 1024 {
            anyhow::bail!("image too large ({} bytes)", bytes.len());
        }
        let mime = mime_for(&path);
        let b64 = base64::engine::general_purpose::STANDARD.encode(&bytes);
        let question = args
            .prompt
            .unwrap_or_else(|| "Describe this image in detail.".into());

        let model = args
            .model
            .or_else(|| non_empty_env("PIRS_VISION_MODEL"))
            .unwrap_or_else(|| {
                // Prefer dashscope VL if DASHSCOPE key present
                if non_empty_env("DASHSCOPE_API_KEY").is_some() {
                    "qwen-vl-plus".into()
                } else {
                    "gpt-4o-mini".into()
                }
            });

        let (base, key) = resolve_openai_compat(Some(&model));
        // For VL on dashscope, use compatible-mode endpoint if only DASHSCOPE set
        let base = base.or_else(|| {
            if non_empty_env("DASHSCOPE_API_KEY").is_some() {
                Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into())
            } else {
                None
            }
        });
        let key = key.or_else(|| non_empty_env("DASHSCOPE_API_KEY"));
        let provider: Arc<dyn pirs_ai::LlmProvider> =
            Arc::new(OpenAiCompat::new(base).with_max_retries(1));

        let user = Message::User(UserMessage {
            content: UserContent::Blocks(vec![
                ContentBlock::Text {
                    text: question,
                    text_signature: None,
                },
                ContentBlock::Image {
                    data: b64,
                    mime_type: mime.into(),
                },
            ]),
            timestamp: 0,
        });
        let ctx_ai = Context {
            system_prompt: Some("You are a careful vision assistant. Be concrete.".into()),
            messages: vec![user],
            tools: vec![],
        };
        let opts = CompletionOptions {
            api_key: key,
            max_tokens: Some(1024),
            ..Default::default()
        };
        let stream = provider
            .stream(
                &model,
                &ctx_ai,
                &opts,
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        let mut text = String::new();
        let mut stream = std::pin::pin!(stream);
        while let Some(ev) = stream.next().await {
            match ev {
                StreamEvent::TextDelta(d) => text.push_str(&d),
                StreamEvent::Done(msg) => {
                    let t = msg.text();
                    if !t.is_empty() {
                        text = t;
                    }
                    break;
                }
                StreamEvent::Error(e) => anyhow::bail!("vision model error: {e}"),
                _ => {}
            }
        }
        if text.trim().is_empty() {
            anyhow::bail!("vision model returned empty description");
        }
        Ok(ToolOutput::text(text))
    }
}

fn mime_for(path: &std::path::Path) -> &'static str {
    match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "webp" => "image/webp",
        "gif" => "image/gif",
        _ => "image/png",
    }
}

pub fn vision_tools(cwd: PathBuf) -> Vec<Arc<dyn AgentTool>> {
    vec![Arc::new(VisionDescribeTool::new(cwd))]
}
