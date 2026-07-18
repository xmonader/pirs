use std::sync::Arc;

use pirs_rhai::ExtensionHost;
use serde_json::json;

fn load(name: &str, with_runner: bool) -> Arc<ExtensionHost> {
    let path = format!(
        "{}/../../examples/extensions/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
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

fn user_msg(t: &str) -> pirs_ai::Message {
    pirs_ai::Message::user(t)
}

#[test]
fn approval_blocks_then_allows_after_user_approval() {
    let host = load("approval.rhai", false);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let transform = hooks.transform_context.unwrap();

    let blocked = before("1", "bash", &json!({"command": "rm -rf /tmp/x"})).unwrap();
    assert!(blocked.contains("APPROVAL REQUIRED (#1)"), "{blocked}");

    assert!(
        before("2", "read", &json!({"path": "f"})).is_none(),
        "safe tools pass"
    );

    assert!(
        before("3", "bash", &json!({"command": "rm -rf /tmp/x"})).is_some(),
        "still blocked before approval"
    );

    transform(vec![user_msg("go do it")]);
    transform(vec![user_msg("approve 1")]);

    assert!(
        before("4", "bash", &json!({"command": "rm -rf /tmp/x"})).is_none(),
        "approved call passes"
    );
}

#[test]
fn approval_approve_all_unlocks_session() {
    let host = load("approval.rhai", false);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let transform = hooks.transform_context.unwrap();

    assert!(before("1", "bash", &json!({"command": "pip install x"})).is_some());
    transform(vec![user_msg("approve all")]);
    assert!(before("2", "bash", &json!({"command": "pip install x"})).is_none());
    assert!(before("3", "bash", &json!({"command": "apt install y"})).is_none());
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
fn checkpoint_snapshots_and_restores() {
    let _guard = ENV_LOCK.lock().unwrap();
    let host = load("checkpoint.rhai", false);
    let tmp = std::env::temp_dir().join(format!("pirs-ckpt-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();

    let cwd = std::env::current_dir().unwrap();
    let _ = std::fs::remove_dir_all(cwd.join(".pirs/checkpoints"));
    for i in 0..5 {
        transform(vec![user_msg(&format!("m{i}"))]);
    }
    let log = cwd.join(".pirs/checkpoints/log.jsonl");
    let content = std::fs::read_to_string(&log).expect("snapshot written");
    assert!(
        content.contains("\"n\":5") || content.contains("\"n\": 5"),
        "{content}"
    );

    assert_eq!(
        host.run_command("checkpoints", "")
            .unwrap()
            .matches("ts=")
            .count(),
        1
    );
    let out = host.run_command("restore", "0").unwrap();
    assert!(out.contains("Checkpoint #0"), "{out}");
    let steering = hooks.get_steering_messages.unwrap();
    let msgs = steering();
    assert_eq!(msgs.len(), 1, "restore pin injected once");
    let _ = std::fs::remove_dir_all(cwd.join(".pirs/checkpoints"));
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
