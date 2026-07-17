use std::sync::Arc;

use pirs_agent::AgentTool;
use pirs_ai::{ContentBlock, ToolResultMessage};
use pirs_rhai::ExtensionHost;
use serde_json::json;

const SCRIPT: &str = r#"
register_tool("greet", "Greet someone", #{
    type: "object",
    properties: #{ name: #{ type: "string" } },
    required: ["name"]
});

fn tool_greet(args) {
    "Hello, " + args.name + "!"
}

fn on_tool_call(id, name, args) {
    if name == "bash" && args.command.contains("rm -rf") {
        return #{ block: true, reason: "destructive command rejected by policy" };
    }
    ()
}

fn on_tool_result(id, name, result) {
    if name == "greet" {
        return #{ text: result.text + " (patched)" };
    }
    ()
}
"#;

fn host() -> Arc<ExtensionHost> {
    let mut host = ExtensionHost::new();
    host.load_source(SCRIPT, "test.rhai".into()).unwrap();
    Arc::new(host)
}

async fn exec(tool: &Arc<dyn AgentTool>, args: serde_json::Value) -> anyhow::Result<pirs_agent::ToolOutput> {
    tool.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args,
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    })
    .await
}

#[tokio::test]
async fn registered_tool_executes_script_function() {
    let host = host();
    let tools = host.tools();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name(), "greet");
    let out = exec(&tools[0], json!({"name": "pi"})).await.unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "Hello, pi!");
}

#[tokio::test]
async fn on_tool_call_blocks() {
    let host = host();
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let blocked = before("1", "bash", &json!({"command": "rm -rf /"}));
    assert_eq!(
        blocked.as_deref(),
        Some("destructive command rejected by policy")
    );
    let allowed = before("2", "bash", &json!({"command": "ls"}));
    assert!(allowed.is_none());
}

#[tokio::test]
async fn on_tool_result_patches_text() {
    let host = host();
    let hooks = host.hooks();
    let after = hooks.after_tool_call.unwrap();
    let result = ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "greet".into(),
        content: vec![ContentBlock::text("Hello, pi!")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    let patch = after("1", "greet", &result).unwrap();
    assert_eq!(
        patch.content.unwrap()[0].as_text().unwrap(),
        "Hello, pi! (patched)"
    );
}

#[test]
fn missing_tool_function_is_load_error() {
    let mut host = ExtensionHost::new();
    let err = host
        .load_source(r#"register_tool("x", "d", #{});"#, "bad.rhai".into())
        .unwrap_err();
    assert!(err.to_string().contains("fn tool_x(args)"));
}
