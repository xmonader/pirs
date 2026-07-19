use std::collections::HashMap;

use pirs_agent::AgentTool;
use pirs_lsp::client::LspClient;
use pirs_lsp::tool::LspTool;

fn rust_analyzer_available() -> bool {
    std::process::Command::new("rust-analyzer")
        .arg("--version")
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// rust-analyzer indexes a crate asynchronously, so a cross-file query issued
/// right after `open_document` can return an empty result until indexing
/// completes — a race that only bites under concurrent load (several servers
/// competing for CPU). Poll `produce` until its text contains `needle`, up to
/// ~30s, mirroring how a real LSP client waits for readiness. `produce` returns
/// the query's text payload (already extracted), so this works for both the raw
/// client (serialized JSON) and the tool (rendered output).
async fn poll_until<F, Fut>(needle: &str, mut produce: F) -> String
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = String>,
{
    for _ in 0..60 {
        let text = produce().await;
        if text.contains(needle) {
            return text;
        }
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
    }
    // One last try so the assertion failure carries the real payload.
    produce().await
}

/// Serialize a client query's result to a JSON string for `poll_until`.
async fn json_of(
    fut: impl std::future::Future<Output = anyhow::Result<serde_json::Value>>,
) -> String {
    match fut.await {
        Ok(v) => serde_json::to_string(&v).unwrap(),
        Err(_) => String::new(),
    }
}

fn fixture_crate() -> tempfile::TempDir {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("Cargo.toml"),
        "[package]\nname = \"fixture\"\nversion = \"0.1.0\"\nedition = \"2021\"\n",
    )
    .unwrap();
    std::fs::create_dir(dir.path().join("src")).unwrap();
    std::fs::write(
        dir.path().join("src/lib.rs"),
        "pub fn target_fn(x: i32) -> i32 {\n    x * 2\n}\n\npub fn unused() {}\n",
    )
    .unwrap();
    std::fs::write(
        dir.path().join("src/main.rs"),
        "fn main() {\n    let v = fixture::target_fn(21);\n    println!(\"{v}\");\n}\n",
    )
    .unwrap();
    dir
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lsp_definition_across_files() {
    if !rust_analyzer_available() {
        eprintln!("rust-analyzer not available, skipping");
        return;
    }
    let dir = fixture_crate();
    let client = LspClient::spawn("rust-analyzer", &[], dir.path())
        .await
        .unwrap();

    let main_rs = dir.path().join("src/main.rs");
    client.open_document(&main_rs, "rust").await.unwrap();
    // Wait for cross-file indexing before asserting (see `poll_until`).
    let text = poll_until("lib.rs", || json_of(client.definition(&main_rs, 2, 22))).await;
    assert!(
        text.contains("lib.rs"),
        "definition should point to lib.rs: {text}"
    );
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lsp_references_and_symbols() {
    if !rust_analyzer_available() {
        eprintln!("rust-analyzer not available, skipping");
        return;
    }
    let dir = fixture_crate();
    let client = LspClient::spawn("rust-analyzer", &[], dir.path())
        .await
        .unwrap();

    client
        .open_document(&dir.path().join("src/lib.rs"), "rust")
        .await
        .unwrap();
    client
        .open_document(&dir.path().join("src/main.rs"), "rust")
        .await
        .unwrap();
    let lib = dir.path().join("src/lib.rs");
    // Wait for cross-file indexing before asserting (see `poll_until`).
    let text = poll_until("main.rs", || json_of(client.references(&lib, 1, 8))).await;
    assert!(
        text.contains("main.rs"),
        "references should include main.rs: {text}"
    );

    let text = poll_until("target_fn", || json_of(client.document_symbols(&lib))).await;
    assert!(text.contains("target_fn"), "{text}");
    client.shutdown().await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn lsp_tool_end_to_end() {
    if !rust_analyzer_available() {
        eprintln!("rust-analyzer not available, skipping");
        return;
    }
    let dir = fixture_crate();
    let tool = LspTool::new(dir.path().to_path_buf());
    // Run one tool action and return its rendered text (empty on error/cancel).
    let run = |args: serde_json::Value| async {
        match tool
            .execute(pirs_agent::ToolExecContext {
                tool_call_id: "t".into(),
                args,
                cancel: tokio_util::sync::CancellationToken::new(),
                on_update: None,
            })
            .await
        {
            Ok(out) => out.content[0]
                .as_text()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            Err(_) => String::new(),
        }
    };

    // Wait for cross-file indexing before asserting (see `poll_until`).
    let def = serde_json::json!({
        "action": "definition", "path": "src/main.rs", "line": 2, "character": 22
    });
    let text = poll_until("src/lib.rs", || run(def.clone())).await;
    assert!(text.contains("src/lib.rs"), "{text}");

    let syms = serde_json::json!({"action": "symbols", "path": "src/lib.rs"});
    let text = poll_until("target_fn", || run(syms.clone())).await;
    assert!(text.contains("target_fn"), "{text}");
    tool.shutdown_all().await;
}

#[test]
fn smoke_no_unused_imports() {
    let _ = std::marker::PhantomData::<HashMap<String, String>>;
}
