use pirs_rhai::ExtensionHost;
use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_map_runs_bound_and_ordered() {
    let mut host = pirs_rhai::ExtensionHost::new();
    host.set_subagent_runner(Arc::new(|task: String, model: Option<String>| {
        std::thread::sleep(std::time::Duration::from_millis(50));
        Ok(format!("[{}]{}", model.unwrap_or_default(), task))
    }));
    host.load_source(
        r#"
register_tool("fan", "fan out", #{ type: "object" });
fn tool_fan(args) {
    parallel_map(["a", "b", "c", "d", "e"], 2, "", "weak")
}
"#,
        "w.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let out = host.tools()[0]
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    let arr = out.content[0].as_text().unwrap().replace(['\n', ' '], "");
    for x in ["a", "b", "c", "d", "e"] {
        assert!(arr.contains(&format!("\"[weak]{x}\"")), "{arr}");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn parallel_map_applies_mapper_fn() {
    let mut host = pirs_rhai::ExtensionHost::new();
    host.set_subagent_runner(Arc::new(|task: String, _| Ok(task)));
    host.load_source(
        r#"
fn build(item) {
    `do ${item}`
}
register_tool("fan", "fan", #{ type: "object" });
fn tool_fan(args) {
    parallel_map(["x", "y"], 2, "build", "")
}
"#,
        "w2.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let out = host.tools()[0]
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    let text = out.content[0].as_text().unwrap().replace('\n', " ");
    assert!(text.contains("\"do x\""), "{text}");
    assert!(text.contains("\"do y\""), "{text}");
}

#[test]
fn cache_roundtrip() {
    let mut host = ExtensionHost::new();
    host.load_source(
        r#"
register_tool("c", "c", #{ type: "object" });
fn tool_c(args) {
    cache_put("k1", #{ answer: 42, items: [1, 2] });
    cache_get("k1")
}
"#,
        "c.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(host.tools()[0].execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    let text = out.content[0].as_text().unwrap().replace(['\n', ' '], "");
    assert!(text.contains("\"answer\":42"), "{text}");
}
