use serde::{Deserialize, Serialize};

pub mod anthropic;
pub mod backends_builtin;
pub mod catalog;
pub mod embed;
pub mod env_auth;
pub mod model_ref;
pub mod openai;
pub mod pricing;
pub mod registry_file;
pub mod routing;
pub mod speech;
pub mod sse;
pub use anthropic::AnthropicClient;
pub use backends_builtin::{
    active_backends, active_portable_models, backend_key_present, builtin_registry,
    dashscope_coding_user_agent, default_user_agent, is_dashscope_coding_url,
};
pub use catalog::{
    catalog_status, load_catalog, refresh_active, refresh_backend, search_catalogs, CatalogFile,
    CatalogModel,
};
pub use embed::{cosine, EmbeddingClient};
pub use env_auth::{non_empty_env, resolve_openai_compat, well_known_key_envs};
pub use model_ref::{format_pin, parse_pin, ModelSpec};
pub use openai::OpenAiCompat;
pub use registry_file::{
    api_key_for_alias, build_routing_provider, expected_key_envs, first_available_backend_key,
    load_user_registry, merge as merge_registry, parse_from_config_value, registry_file_has_models,
    user_config_path, BackendEntry, ModelEntry, RegistryFile, ServeEntry,
};
pub use routing::{BackendKind, BackendSpec, ModelRoute, RoutingProvider, ServeTarget};
pub use speech::{
    env_speech_endpoints, probe_speech_base_health, resolve_speech_route, resolve_speech_route_in,
    speak_text, speak_with_failover, speech_status_lines, speech_status_lines_probed,
    transcribe_path, transcribe_with_failover, write_audio_file, SpeakOptions, SpeechClient,
    SpeechEndpoint, SpeechKind, SpeechRoute, TranscribeOptions,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
#[derive(Default)]
pub enum StopReason {
    #[default]
    Stop,
    Length,
    ToolUse,
    Error,
    Aborted,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct Usage {
    pub input: u64,
    pub output: u64,
    pub cache_read: u64,
    pub cache_write: u64,
    pub total_tokens: u64,
    pub reasoning: u64,
}

impl std::ops::AddAssign for Usage {
    fn add_assign(&mut self, other: Usage) {
        self.input += other.input;
        self.output += other.output;
        self.cache_read += other.cache_read;
        self.cache_write += other.cache_write;
        self.total_tokens += other.total_tokens;
        self.reasoning += other.reasoning;
    }
}

impl std::ops::Add for Usage {
    type Output = Usage;
    fn add(mut self, other: Usage) -> Usage {
        self += other;
        self
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum ContentBlock {
    #[serde(rename = "text")]
    Text {
        text: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        text_signature: Option<String>,
    },
    #[serde(rename = "thinking")]
    Thinking {
        thinking: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        thinking_signature: Option<String>,
        #[serde(default)]
        redacted: bool,
    },
    #[serde(rename = "image")]
    Image { data: String, mime_type: String },
    #[serde(rename = "toolCall")]
    ToolCall {
        id: String,
        name: String,
        arguments: serde_json::Value,
        #[serde(skip_serializing_if = "Option::is_none")]
        thought_signature: Option<String>,
    },
}

impl ContentBlock {
    pub fn text(s: impl Into<String>) -> Self {
        ContentBlock::Text {
            text: s.into(),
            text_signature: None,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ContentBlock::Text { text, .. } => Some(text),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum UserContent {
    Text(String),
    Blocks(Vec<ContentBlock>),
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UserMessage {
    pub content: UserContent,
    #[serde(default = "now_millis")]
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct AssistantMessage {
    pub content: Vec<ContentBlock>,
    pub api: String,
    pub provider: String,
    pub model: String,
    pub usage: Usage,
    pub stop_reason: StopReason,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error_message: Option<String>,
    #[serde(default = "now_millis")]
    pub timestamp: u64,
}

impl Default for AssistantMessage {
    fn default() -> Self {
        AssistantMessage {
            content: Vec::new(),
            api: String::new(),
            provider: String::new(),
            model: String::new(),
            usage: Usage::default(),
            stop_reason: StopReason::Stop,
            error_message: None,
            timestamp: now_millis(),
        }
    }
}

impl AssistantMessage {
    pub fn text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("")
    }

    pub fn tool_calls(&self) -> Vec<&ContentBlock> {
        self.content
            .iter()
            .filter(|b| matches!(b, ContentBlock::ToolCall { .. }))
            .collect()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolResultMessage {
    pub tool_call_id: String,
    pub tool_name: String,
    pub content: Vec<ContentBlock>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub details: Option<serde_json::Value>,
    #[serde(default)]
    pub is_error: bool,
    #[serde(default)]
    pub terminate: bool,
    #[serde(default = "now_millis")]
    pub timestamp: u64,
}

impl ToolResultMessage {
    /// Model-facing text (what is in `content` — already capped for history).
    pub fn model_text(&self) -> String {
        self.content
            .iter()
            .filter_map(|b| b.as_text())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Prefer longer UI text from `details.uiText` when present; else model text.
    /// Interactive surfaces (REPL/TUI) should use this for display.
    pub fn display_text(&self) -> String {
        if let Some(ui) = self
            .details
            .as_ref()
            .and_then(|d| d.get("uiText"))
            .and_then(|v| v.as_str())
        {
            if !ui.is_empty() {
                return ui.to_string();
            }
        }
        self.model_text()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "role", rename_all = "camelCase")]
pub enum Message {
    #[serde(rename = "user")]
    User(UserMessage),
    #[serde(rename = "assistant")]
    Assistant(AssistantMessage),
    #[serde(rename = "toolResult")]
    ToolResult(ToolResultMessage),
}

impl Message {
    pub fn user(text: impl Into<String>) -> Self {
        Message::User(UserMessage {
            content: UserContent::Text(text.into()),
            timestamp: now_millis(),
        })
    }

    pub fn is_assistant(&self) -> bool {
        matches!(self, Message::Assistant(_))
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDef {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
}

#[derive(Debug, Clone, Default)]
pub struct Context {
    pub system_prompt: Option<String>,
    pub messages: Vec<Message>,
    pub tools: Vec<ToolDef>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolChoice {
    Auto,
    None,
    Required,
}

#[derive(Debug, Clone, Default)]
pub struct CompletionOptions {
    pub api_key: Option<String>,
    pub temperature: Option<f64>,
    pub max_tokens: Option<u64>,
    pub tool_choice: Option<ToolChoice>,
    pub reasoning_effort: Option<String>,
    pub extra_headers: Vec<(String, String)>,
    pub timeout: Option<std::time::Duration>,
}

#[derive(Debug, Clone)]
pub enum StreamEvent {
    Start,
    TextDelta(String),
    ThinkingDelta(String),
    ToolCallDelta,
    Done(Box<AssistantMessage>),
    Error(String),
}

#[derive(Debug, thiserror::Error)]
pub enum AiError {
    #[error("http error {status}: {body}")]
    Http { status: u16, body: String },
    #[error("network error: {0}")]
    Network(#[from] reqwest::Error),
    #[error("stream error: {0}")]
    Stream(String),
    #[error("decode error: {0}")]
    Decode(String),
}

#[async_trait::async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream(
        &self,
        model: &str,
        context: &Context,
        options: &CompletionOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::stream::BoxStream<'static, StreamEvent>;
}

pub mod retry {
    use tokio_util::sync::CancellationToken;

    /// Maximum wait regardless of Retry-After, so a hostile or broken gateway
    /// cannot park a task for hours.
    pub const MAX_RETRY_SECS: u64 = 120;

    /// Pure wait-duration computation (testable without sleeping).
    pub fn backoff_duration(attempt: u32, retry_after: Option<u64>) -> std::time::Duration {
        let secs = retry_after
            .unwrap_or_else(|| 1u64 << attempt.min(5))
            .min(MAX_RETRY_SECS);
        let jitter_ms = crate::now_millis() % 1000;
        std::time::Duration::from_secs(secs) + std::time::Duration::from_millis(jitter_ms)
    }

    /// Shared backoff: honors Retry-After (capped), exponential otherwise, with jitter.
    pub async fn backoff(attempt: u32, retry_after: Option<u64>, cancel: &CancellationToken) {
        let wait = backoff_duration(attempt, retry_after);
        tokio::select! {
            _ = cancel.cancelled() => {}
            _ = tokio::time::sleep(wait) => {}
        }
    }

    pub fn extract_error_body(body: &str) -> String {
        const MAX: usize = 4000;
        let truncated: String = body.chars().take(MAX).collect();
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&truncated) {
            if let Some(err) = v.get("error") {
                if let Some(m) = err.get("message").and_then(|m| m.as_str()) {
                    return m.to_string();
                }
                return err.to_string();
            }
        }
        truncated
    }
}

pub fn now_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}
