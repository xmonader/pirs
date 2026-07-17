use std::sync::Arc;

use pirs_ai::{AssistantMessage, ContentBlock, Message, ToolResultMessage};
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub type Emit = Arc<dyn Fn(AgentEvent) + Send + Sync>;
pub type BeforeToolCallHook = Arc<dyn Fn(&str, &str, &Value) -> Option<String> + Send + Sync>;
pub type AfterToolCallHook =
    Arc<dyn Fn(&str, &str, &ToolResultMessage) -> Option<ToolResultPatch> + Send + Sync>;
pub type TransformContextHook = Arc<dyn Fn(Vec<Message>) -> Vec<Message> + Send + Sync>;
pub type ShouldStopHook = Arc<dyn Fn(&pirs_ai::Context) -> bool + Send + Sync>;
pub type MessageSourceHook = Arc<dyn Fn() -> Vec<Message> + Send + Sync>;
pub type ApiKeyHook = Arc<dyn Fn() -> Option<String> + Send + Sync>;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", rename_all_fields = "camelCase")]
pub enum AgentEvent {
    AgentStart,
    AgentEnd { messages: Vec<Message> },
    TurnStart,
    TurnEnd { message: Box<AssistantMessage>, tool_results: Vec<ToolResultMessage> },
    MessageStart { message: Box<Message> },
    MessageUpdate { message: Box<AssistantMessage> },
    MessageEnd { message: Box<Message> },
    ToolExecutionStart { tool_call_id: String, tool_name: String, args: Value },
    ToolExecutionUpdate { tool_call_id: String, tool_name: String, partial: String },
    ToolExecutionEnd { tool_call_id: String, tool_name: String, result: Box<ToolResultMessage> },
}

#[derive(Debug, Clone, Default)]
pub struct ToolResultPatch {
    pub content: Option<Vec<ContentBlock>>,
    pub details: Option<Value>,
    pub is_error: Option<bool>,
    pub terminate: Option<bool>,
}

#[derive(Default, Clone)]
pub struct Hooks {
    pub before_tool_call: Option<BeforeToolCallHook>,
    pub after_tool_call: Option<AfterToolCallHook>,
    pub transform_context: Option<TransformContextHook>,
    pub should_stop_after_turn: Option<ShouldStopHook>,
    pub get_steering_messages: Option<MessageSourceHook>,
    pub get_follow_up_messages: Option<MessageSourceHook>,
    pub get_api_key: Option<ApiKeyHook>,
}

impl Hooks {
    pub fn steering(&self) -> Vec<Message> {
        self.get_steering_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default()
    }

    pub fn follow_up(&self) -> Vec<Message> {
        self.get_follow_up_messages
            .as_ref()
            .map(|f| f())
            .unwrap_or_default()
    }
}
