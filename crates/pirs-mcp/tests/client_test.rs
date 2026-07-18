use pirs_mcp::client::StdioClient;
use std::collections::HashMap;

fn script() -> String {
    format!("{}/tests/mcp_echo.py", env!("CARGO_MANIFEST_DIR"))
}

async fn spawn() -> std::sync::Arc<StdioClient> {
    StdioClient::spawn("echo", "python3", &[script()], &HashMap::new(), None)
        .await
        .unwrap()
}

async fn spawn_facade() -> std::sync::Arc<pirs_mcp::client::Client> {
    std::sync::Arc::new(pirs_mcp::client::Client::Stdio(spawn().await))
}

#[tokio::test]
async fn initialize_list_and_call() {
    let client = spawn().await;
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
    let client = spawn().await;
    let fail = client
        .call_tool("fail", serde_json::json!({}))
        .await
        .unwrap();
    assert!(fail.is_error);
    assert_eq!(fail.content[0].as_text().unwrap(), "intentional failure");
    client.shutdown().await;
}

#[tokio::test]
async fn unknown_tool_returns_error() {
    let client = spawn().await;
    let err = client
        .call_tool("nope", serde_json::json!({}))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("unknown tool"));
    client.shutdown().await;
}

#[tokio::test]
async fn mcp_tool_as_agent_tool() {
    let client = spawn_facade().await;
    let defs = client.list_tools().await.unwrap();
    let tool: std::sync::Arc<dyn pirs_agent::AgentTool> =
        pirs_mcp::tool::McpTool::new("echo-srv", defs[0].clone(), client);
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
    match StdioClient::spawn("missing", "/nonexistent/binary", &[], &HashMap::new(), None).await {
        Ok(_) => panic!("spawn should fail"),
        Err(e) => assert!(e.to_string().contains("failed to spawn MCP server")),
    }
}
