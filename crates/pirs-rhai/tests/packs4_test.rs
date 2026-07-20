use std::sync::Arc;

use pirs_rhai::ExtensionHost;
use serde_json::json;

fn load(name: &str, with_runner: bool) -> Arc<ExtensionHost> {
    let path = format!("{}/../../extensions/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut host = ExtensionHost::new();
    if with_runner {
        host.set_subagent_runner(Arc::new(|task: String, model: Option<String>| {
            Ok(format!(
                "[{}] {}",
                model.unwrap_or_else(|| "default".into()),
                &task[..task.len().min(80)]
            ))
        }));
    }
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn subagents_registers_persona_tools_from_files() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-agents-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let agents = tmp.join(".pirs/agents");
    std::fs::create_dir_all(&agents).unwrap();
    std::fs::write(
        agents.join("reviewer.md"),
        "---\nname: reviewer\ndescription: Reviews code brutally\nmodel: glm-4.7\n---\nYou attack code.\n",
    )
    .unwrap();

    let host = load("subagents.rhai", true);
    let tools = host.tools();
    let reviewer = tools
        .iter()
        .find(|t| t.name() == "reviewer")
        .expect("reviewer tool");
    assert_eq!(reviewer.description(), "Reviews code brutally");

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(reviewer.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"task": "review fizzbuzz.py"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    let text = out.content[0].as_text().unwrap();
    assert!(text.contains("[glm-4.7]"), "persona model routed: {text}");
    assert!(
        text.contains("'reviewer' specialist"),
        "persona framing included: {text}"
    );
}

#[test]
fn web_fetch_reads_file_urls() {
    let host = load("web-tools.rhai", false);
    let tools = host.tools();
    let fetch = tools.iter().find(|t| t.name() == "web_fetch").unwrap();

    let tmp = std::env::temp_dir().join("pirs-web-test.txt");
    std::fs::write(&tmp, "hello from disk").unwrap();
    let url = format!("file://{}", tmp.display());

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(fetch.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"url": url}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out.content[0]
        .as_text()
        .unwrap()
        .contains("hello from disk"));
}
