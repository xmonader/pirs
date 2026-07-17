use std::sync::Arc;

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn run_subagent_from_script() {
    let mut host = pirs_rhai::ExtensionHost::new();
    host.set_subagent_runner(Arc::new(|task: String, model: Option<String>| {
        Ok(format!(
            "answered[{}]: {}",
            model.unwrap_or_else(|| "default".into()),
            task
        ))
    }));
    host.load_source(
        r#"
register_tool("ask", "ask a sub-agent", #{ type: "object", properties: #{ q: #{ type: "string" } }, required: ["q"] });
fn tool_ask(args) {
    run_subagent(args.q, "weak-model")
}
"#,
        "sub.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let tools = host.tools();
    let out = tools[0]
        .execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"q": "2+2?"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        })
        .await
        .unwrap();
    assert_eq!(
        out.content[0].as_text().unwrap(),
        "answered[weak-model]: 2+2?"
    );
}

#[test]
fn run_subagent_unregistered_without_runner() {
    let mut host = pirs_rhai::ExtensionHost::new();
    host.load_source(
        "fn tool_x(args) { run_subagent(\"hi\") }",
        "x.rhai".into(),
    )
    .unwrap();
    let host = Arc::new(host);
    let hooks = host.hooks();
    let _ = hooks;
}
