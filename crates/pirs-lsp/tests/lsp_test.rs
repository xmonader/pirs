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

    client.open_document(&dir.path().join("src/main.rs"), "rust").await.unwrap();
    let result = client
        .definition(&dir.path().join("src/main.rs"), 2, 22)
        .await
        .unwrap();

    let text = serde_json::to_string(&result).unwrap();
    assert!(text.contains("lib.rs"), "definition should point to lib.rs: {text}");
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

    client.open_document(&dir.path().join("src/lib.rs"), "rust").await.unwrap();
    client.open_document(&dir.path().join("src/main.rs"), "rust").await.unwrap();
    let refs = client
        .references(&dir.path().join("src/lib.rs"), 1, 8)
        .await
        .unwrap();
    let text = serde_json::to_string(&refs).unwrap();
    assert!(text.contains("main.rs"), "references should include main.rs: {text}");

    let syms = client
        .document_symbols(&dir.path().join("src/lib.rs"))
        .await
        .unwrap();
    let text = serde_json::to_string(&syms).unwrap();
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
    let out = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({
                "action": "definition",
                "path": "src/main.rs",
                "line": 2,
                "character": 22
            }),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert!(out.content[0].as_text().unwrap().contains("src/lib.rs"));

    let out2 = tool
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"action": "symbols", "path": "src/lib.rs"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert!(out2.content[0].as_text().unwrap().contains("target_fn"));
    tool.shutdown_all().await;
}

#[test]
fn smoke_no_unused_imports() {
    let _ = std::marker::PhantomData::<HashMap<String, String>>;
}
