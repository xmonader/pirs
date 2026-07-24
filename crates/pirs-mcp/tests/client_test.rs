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

fn email_calendar_script() -> String {
    format!(
        "{}/tests/mcp_email_calendar.py",
        env!("CARGO_MANIFEST_DIR")
    )
}

/// End-to-end product path: `~/.pirs/mcp.json` is always trusted → load_servers
/// registers email/calendar tools (OpenClaw/Hermes MCP connector parity).
#[tokio::test]
async fn email_calendar_via_user_mcp_json_load_servers() {
    use pirs_mcp::config::{load_server_specs_with_trust, ServerTransport};

    let dir = tempfile::tempdir().unwrap();
    let script = email_calendar_script();
    let cfg = serde_json::json!({
        "mcpServers": {
            "email-calendar": {
                "command": "python3",
                "args": [script]
            }
        }
    });
    // Spec parse with forced trust (project path shape).
    std::fs::write(dir.path().join(".mcp.json"), cfg.to_string()).unwrap();
    let (specs, errors) = load_server_specs_with_trust(dir.path(), &mut |_| true);
    assert!(errors.is_empty(), "{errors:?}");
    assert_eq!(specs.len(), 1);
    assert_eq!(specs[0].name, "email-calendar");
    match &specs[0].transport {
        ServerTransport::Stdio { command, args, .. } => {
            assert_eq!(command, "python3");
            assert!(
                args.iter().any(|a| a.contains("mcp_email_calendar")),
                "{args:?}"
            );
        }
        _ => panic!("expected stdio"),
    }

    // User-global config is trusted without TTY (product path for personal connectors).
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    std::fs::write(home.join(".pirs").join("mcp.json"), cfg.to_string()).unwrap();
    let cwd = dir.path().join("empty-project");
    std::fs::create_dir_all(&cwd).unwrap();
    let prev_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);
    let result = pirs_mcp::load_servers(&cwd).await;
    if let Some(h) = prev_home {
        std::env::set_var("HOME", h);
    } else {
        std::env::remove_var("HOME");
    }
    assert!(
        result.errors.is_empty(),
        "user mcp.json should load cleanly: {:?}",
        result.errors
    );
    assert!(
        result.tools.len() >= 4,
        "expected email+calendar tools, got {} tools {:?}",
        result.tools.len(),
        result.tools.iter().map(|t| t.name().to_string()).collect::<Vec<_>>()
    );
    let names: Vec<_> = result.tools.iter().map(|t| t.name().to_string()).collect();
    assert!(
        names.iter().any(|n| n.contains("email_list")),
        "{names:?}"
    );
    assert!(
        names.iter().any(|n| n.contains("calendar_list")),
        "{names:?}"
    );
    let rep = pirs_mcp::McpDegradedReport::from_load(&result);
    assert!(rep.is_fully_healthy(), "{:?}", rep.lines());
    assert!(rep.lines().iter().any(|l| l.contains("ok: email-calendar")));
}

#[tokio::test]
async fn email_calendar_list_and_read_tools() {
    let client = StdioClient::spawn(
        "email-calendar",
        "python3",
        &[email_calendar_script()],
        &HashMap::new(),
        None,
    )
    .await
    .unwrap();
    let tools = client.list_tools().await.unwrap();
    let names: Vec<_> = tools.iter().map(|t| t.name.as_str()).collect();
    assert!(names.contains(&"email_list"), "{names:?}");
    assert!(names.contains(&"email_read"), "{names:?}");
    assert!(names.contains(&"calendar_list"), "{names:?}");
    assert!(names.contains(&"calendar_get"), "{names:?}");

    let list = client
        .call_tool("email_list", serde_json::json!({"limit": 5}))
        .await
        .unwrap();
    assert!(!list.is_error);
    let body = list.content[0].as_text().unwrap();
    assert!(body.contains("Q3 plan") || body.contains("alice@example.com"), "{body}");

    let msg = client
        .call_tool("email_read", serde_json::json!({"id": "m1"}))
        .await
        .unwrap();
    assert!(!msg.is_error);
    assert!(
        msg.content[0].as_text().unwrap().contains("Q3 plan"),
        "{:?}",
        msg.content[0].as_text()
    );

    let events = client
        .call_tool("calendar_list", serde_json::json!({"days": 7}))
        .await
        .unwrap();
    assert!(!events.is_error);
    let ev_body = events.content[0].as_text().unwrap();
    assert!(ev_body.contains("Standup"), "{ev_body}");

    let ev = client
        .call_tool("calendar_get", serde_json::json!({"id": "e1"}))
        .await
        .unwrap();
    assert!(!ev.is_error);
    assert!(ev.content[0].as_text().unwrap().contains("Standup"));

    // Agent-facing tool name prefix
    let facade = std::sync::Arc::new(pirs_mcp::client::Client::Stdio(client));
    let defs = facade.list_tools().await.unwrap();
    let email_list = defs.iter().find(|d| d.name == "email_list").unwrap();
    let tool: std::sync::Arc<dyn pirs_agent::AgentTool> =
        pirs_mcp::tool::McpTool::new("email-calendar", email_list.clone(), facade);
    assert_eq!(tool.name(), "mcp_email-calendar_email_list");
    let out = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    let text = out.content[0].as_text().unwrap();
    assert!(text.contains("messages") || text.contains("Q3"), "{text}");
}
