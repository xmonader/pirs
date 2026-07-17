use pirs_mcp::client::{McpClient, ServerSpec};
use std::collections::HashMap;

fn spec() -> ServerSpec {
    ServerSpec {
        name: "echo".into(),
        command: "python3".into(),
        args: vec![format!("{}/mcp_echo.py", env!("CARGO_MANIFEST_DIR"),).replace("crates/pirs-mcp", "crates/pirs-mcp/tests")],
        env: HashMap::new(),
        cwd: None,
    }
}

#[tokio::test]
async fn initialize_list_and_call() {
    let client = McpClient::spawn(&spec()).await.unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 3);
    assert_eq!(tools[0].name, "echo");
    assert!(tools[1].input_schema["properties"]["a"].is_object());

    let echo = client
        .call_tool("echo", serde_json::json!({"text": "hello mcp"}))
        .await
        .unwrap();
    assert!(!echo.is_error);
    assert_eq!(echo.content[0].as_text().unwrap(), "echo: hello mcp");

    let add = client
        .call_tool("add", serde_json::json!({"a": 2, "b": 40}))
        .await
        .unwrap();
    assert_eq!(add.content[0].as_text().unwrap(), "42");

    client.shutdown().await;
}

#[tokio::test]
async fn error_result_maps_is_error() {
    let client = McpClient::spawn(&spec()).await.unwrap();
    let fail = client.call_tool("fail", serde_json::json!({})).await.unwrap();
    assert!(fail.is_error);
    assert_eq!(fail.content[0].as_text().unwrap(), "intentional failure");
    client.shutdown().await;
}

#[tokio::test]
async fn unknown_tool_returns_error() {
    let client = McpClient::spawn(&spec()).await.unwrap();
    let err = client.call_tool("nope", serde_json::json!({})).await.unwrap_err();
    assert!(err.to_string().contains("unknown tool"));
    client.shutdown().await;
}

#[tokio::test]
async fn mcp_tool_as_agent_tool() {
    let client = McpClient::spawn(&spec()).await.unwrap();
    let defs = client.list_tools().await.unwrap();
    let tool: std::sync::Arc<dyn pirs_agent::AgentTool> = pirs_mcp::tool::McpTool::new("echo-srv", defs[0].clone(), client);
    assert_eq!(tool.name(), "mcp_echo-srv_echo");
    let out = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"text": "via agent"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "echo: via agent");
}

#[tokio::test]
async fn spawn_failure_is_reported() {
    let spec = ServerSpec {
        name: "missing".into(),
        command: "/nonexistent/binary".into(),
        args: vec![],
        env: HashMap::new(),
        cwd: None,
    };
    match McpClient::spawn(&spec).await {
        Ok(_) => panic!("spawn should fail"),
        Err(e) => assert!(e.to_string().contains("failed to spawn MCP server")),
    }
}
