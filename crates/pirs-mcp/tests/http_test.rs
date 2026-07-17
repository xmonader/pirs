use std::collections::HashMap;
use std::process::Stdio;

struct MockServer {
    port: u16,
    child: std::process::Child,
}

impl MockServer {
    async fn start() -> Self {
        for port in 19001..19100 {
            let mut child = std::process::Command::new("python3")
                .arg(format!(
                    "{}/tests/mock_mcp_http.py",
                    env!("CARGO_MANIFEST_DIR")
                ))
                .arg(port.to_string())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let mut stdout = std::io::BufReader::new(child.stdout.take().unwrap());
            use std::io::BufRead;
            let mut line = String::new();
            if stdout.read_line(&mut line).is_ok() && line.contains("ready") {
                return MockServer { port, child };
            }
            let _ = child.kill();
            let _ = child.wait();
        }
        panic!("no free port for mock server");
    }

    fn url(&self, path: &str) -> String {
        format!("http://127.0.0.1:{}{path}", self.port)
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[tokio::test]
async fn streamable_http_json_mode() {
    let server = MockServer::start().await;
    let client = pirs_mcp::http::HttpClient::connect(&server.url("/mcp"), &HashMap::new())
        .await
        .unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools.len(), 1);
    assert_eq!(tools[0].name, "echo");
    let out = client
        .call_tool("echo", serde_json::json!({"text": "over http"}))
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "echo: over http");
    client.shutdown().await;
}

#[tokio::test]
async fn streamable_http_sse_mode() {
    let server = MockServer::start().await;
    let mut headers = HashMap::new();
    headers.insert("x-test-mode".to_string(), "sse".to_string());
    let client = pirs_mcp::http::HttpClient::connect(&server.url("/mcp"), &headers)
        .await
        .unwrap();
    let out = client
        .call_tool("echo", serde_json::json!({"text": "sse answer"}))
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "echo: sse answer");
    client.shutdown().await;
}

#[tokio::test]
async fn legacy_sse_transport() {
    let server = MockServer::start().await;
    let client = pirs_mcp::http::LegacySseClient::connect(&server.url("/sse"), &HashMap::new())
        .await
        .unwrap();
    let tools = client.list_tools().await.unwrap();
    assert_eq!(tools[0].name, "echo");
    let out = client
        .call_tool("echo", serde_json::json!({"text": "legacy works"}))
        .await
        .unwrap();
    assert_eq!(out.content[0].as_text().unwrap(), "echo: legacy works");
    client.shutdown().await;
}
