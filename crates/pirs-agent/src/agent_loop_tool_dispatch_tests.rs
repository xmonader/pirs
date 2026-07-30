use super::freeform::*;
use super::tool_exec::*;
use super::*;
use crate::tool::{AgentTool, ToolExecContext, ToolOutput};
use async_trait::async_trait;
use serde_json::json;
use std::sync::Arc;

struct EchoTool;

#[async_trait]
impl AgentTool for EchoTool {
    fn name(&self) -> &str {
        "echo"
    }
    fn description(&self) -> &str {
        "echo"
    }
    fn parameters(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "msg": { "type": "string" } },
            "required": ["msg"]
        })
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        Ok(ToolOutput::text("ok"))
    }
}

#[test]
fn freeform_markdown_tool_text_detected() {
    let sample = r#"```python
[{"function": "read", "path": "foo.rs"}]
```"#;
    assert!(looks_like_freeform_tool_text(sample));
    assert!(looks_like_freeform_tool_text("> bash ls -la"));
    assert!(!looks_like_freeform_tool_text(
        "I'll fix the bug by editing txn.py"
    ));
}

#[test]
fn invalid_tool_payload_is_rejected_not_success() {
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
    // Unknown tool name
    let err = validate_tool_call_payload("not_a_tool", &json!({}), &tools, &None)
        .expect_err("unknown tool must fail");
    assert!(
        err.contains("not found") || err.contains("not_a_tool"),
        "{err}"
    );
    // Known tool, invalid args (missing required)
    let err = validate_tool_call_payload("echo", &json!({}), &tools, &None)
        .expect_err("missing required arg must fail");
    assert!(
        err.contains("Invalid arguments") || err.to_ascii_lowercase().contains("required"),
        "{err}"
    );
    // Valid payload
    assert!(validate_tool_call_payload("echo", &json!({"msg": "hi"}), &tools, &None).is_ok());
}

#[tokio::test]
async fn execute_invalid_payload_returns_error_result_not_success() {
    let tools: Vec<Arc<dyn AgentTool>> = vec![Arc::new(EchoTool)];
    let emit: Emit = Arc::new(|_| {});
    let results = execute_tool_calls_for_test(
        vec![ToolCallData {
            id: "1".into(),
            name: "echo".into(),
            arguments: json!({"wrong": true}),
        }],
        &tools,
        &Hooks::default(),
        CancellationToken::new(),
        &emit,
        true,
        None,
        None,
    )
    .await;
    assert_eq!(results.len(), 1);
    assert!(
        results[0].is_error,
        "invalid args must not count as successful tool use: {}",
        results[0].model_text()
    );
    assert!(
        results[0].model_text().contains("Invalid arguments")
            || results[0]
                .model_text()
                .to_ascii_lowercase()
                .contains("required"),
        "{}",
        results[0].model_text()
    );
}
