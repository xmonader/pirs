use std::sync::Arc;

use pirs_rhai::ExtensionHost;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

type Runner = Arc<dyn Fn(String, Option<String>) -> Result<String, String> + Send + Sync>;

fn load(name: &str, runner: Option<Runner>) -> Arc<ExtensionHost> {
    let path = format!(
        "{}/../../examples/extensions/{name}",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut host = ExtensionHost::new();
    if let Some(r) = runner {
        host.set_subagent_runner(r);
    }
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

fn ok_runner() -> Runner {
    Arc::new(|task: String, _| Ok(format!("REAL ISSUE in: {}", &task[..task.len().min(30)])))
}

#[test]
fn critic_spawns_background_review_after_n_edits() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-critic-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::fs::write(
        tmp.join("f.txt"),
        "v1
",
    )
    .unwrap();
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::fs::write(
        tmp.join("f.txt"),
        "v2
",
    )
    .unwrap();
    let old_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = load("critic.rhai", Some(ok_runner()));
    let hooks = host.hooks();
    let after = hooks.after_tool_call.unwrap();
    let steering = hooks.get_steering_messages.unwrap();

    let edit = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "edit".into(),
        content: vec![pirs_ai::ContentBlock::text("ok")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    for _ in 0..3 {
        after("1", "edit", &edit);
    }
    let mut msgs = Vec::new();
    for _ in 0..30 {
        msgs = steering();
        if !msgs.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(100));
    }
    assert_eq!(msgs.len(), 1, "critic verdict should steer: {msgs:?}");
    std::env::set_current_dir(old_cwd).unwrap();
}

#[test]
fn approval2_denies_dangerous_approves_safe() {
    let runner: Runner = Arc::new(|task: String, _| {
        if task.contains("rm -rf /important") {
            Ok("DENY - deletes data".to_string())
        } else {
            Ok("APPROVE - fine".to_string())
        }
    });
    let host = load("approval2.rhai", Some(runner));
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();

    let denied = before(
        "1",
        "bash",
        &serde_json::json!({"command": "rm -rf /important"}),
    );
    assert!(denied.is_some(), "judge denied: {denied:?}");

    let allowed = before(
        "2",
        "bash",
        &serde_json::json!({"command": "rm -rf ./build"}),
    );
    assert!(allowed.is_none(), "judge approved: {allowed:?}");

    let pass = before("3", "bash", &serde_json::json!({"command": "ls -la"}));
    assert!(pass.is_none(), "unflagged commands pass: {pass:?}");
}

#[test]
fn crystallizer_writes_skill_after_successful_run() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-crystal-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    std::fs::create_dir_all(tmp.join(".pirs")).unwrap();
    std::fs::write(
        tmp.join(".pirs/audit.jsonl"),
        "{\"kind\":\"call\",\"tool\":\"edit\",\"args\":{\"path\":\"x.rs\"}}\n",
    )
    .unwrap();
    let runner: Arc<dyn Fn(String, Option<String>) -> Result<String, String> + Send + Sync> =
        Arc::new(|_, _| {
            Ok("---\nname: rust-editing\ndescription: How to edit Rust files safely\n---\nAlways run cargo check after edits.\n".to_string())
        });
    let host = load("skill-crystallizer.rhai", Some(runner));
    let listener = host.listener().unwrap();
    let hooks = host.hooks();
    let after = hooks.after_tool_call.unwrap();
    let edit = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "edit".into(),
        content: vec![pirs_ai::ContentBlock::text("ok")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    after("1", "edit", &edit);
    after("2", "edit", &edit);
    listener(pirs_agent::AgentEvent::AgentEnd { messages: vec![] });

    let skill = tmp.join(".pirs/skills/rust-editing/SKILL.md");
    assert!(skill.exists(), "skill should be written to {skill:?}");
    let content = std::fs::read_to_string(&skill).unwrap();
    assert!(content.contains("name: rust-editing"));
}

#[test]
fn rollback_snapshots_and_restores() {
    let _guard = ENV_LOCK.lock().unwrap();
    let host = load("rollback.rhai", None);
    let tmp = std::env::temp_dir().join(format!("pirs-rb-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::process::Command::new("git")
        .args(["init", "-q"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::fs::write(tmp.join("f.txt"), "v1\n").unwrap();
    std::process::Command::new("git")
        .args(["add", "-A"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::process::Command::new("git")
        .args([
            "-c",
            "user.email=t@t",
            "-c",
            "user.name=t",
            "commit",
            "-qm",
            "init",
        ])
        .current_dir(&tmp)
        .output()
        .unwrap();

    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();
    let listener = host.listener().unwrap();
    let hooks = host.hooks();
    let after = hooks.after_tool_call.unwrap();
    let edit = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "edit".into(),
        content: vec![pirs_ai::ContentBlock::text("ok")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    std::fs::write(tmp.join("f.txt"), "v2\n").unwrap();
    after("1", "edit", &edit);
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage::default()),
        tool_results: vec![],
    });
    std::fs::write(tmp.join("f.txt"), "v3\n").unwrap();
    after("2", "edit", &edit);
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage::default()),
        tool_results: vec![],
    });

    let list = host.run_command("snapshots", "").unwrap();
    assert!(list.contains("refs/pirs/turn-0"), "{list}");
    assert!(list.contains("refs/pirs/turn-1"), "{list}");

    let out = host.run_command("undo", "0").unwrap();
    assert!(out.contains("restored"), "{out}");
    assert_eq!(
        std::fs::read_to_string(tmp.join("f.txt")).unwrap(),
        "v2\n",
        "undo to snapshot 0 must rewind to the post-first-edit state"
    );
    std::env::set_current_dir(cwd).unwrap();
}

#[test]
fn swarm_post_claim_done_cycle() {
    let _guard = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-swarm-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let host = load("swarm.rhai", None);
    let tools = host.tools();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let exec = |name: &str, args: serde_json::Value| {
        let tool = tools.iter().find(|t| t.name() == name).unwrap();
        rt.block_on(tool.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args,
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap()
        .content[0]
            .as_text()
            .unwrap()
            .to_string()
    };

    assert!(exec("swarm_claim", serde_json::json!({})).contains("no open packets"));
    exec(
        "swarm_post",
        serde_json::json!({"task": "port module A", "role": "worker"}),
    );
    let claim = exec("swarm_claim", serde_json::json!({}));
    assert!(claim.contains("packet #1: port module A"), "{claim}");
    exec(
        "swarm_done",
        serde_json::json!({"id": 1, "result": "ported"}),
    );
    let status = exec("swarm_status", serde_json::json!({}));
    assert!(status.contains("[done]"), "{status}");
}

#[test]
fn spawn_and_inbox_roundtrip() {
    let mut host = ExtensionHost::new();
    host.set_subagent_runner(Arc::new(|task: String, _| Ok(format!("done: {task}"))));
    host.load_source(
        r#"
fn tool_x(args) {
    spawn_subagent("job1", "", "tag1");
    spawn_subagent("job2", "", "tag2");
    "spawned"
}
register_tool("x", "x", #{ type: "object" });
"#,
        "spawn.rhai".into(),
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
    assert_eq!(out.content[0].as_text().unwrap(), "spawned");
    std::thread::sleep(std::time::Duration::from_millis(300));
    let items = host.inbox_drain();
    let tags: Vec<&str> = items.iter().map(|(t, _)| t.as_str()).collect();
    assert!(tags.contains(&"tag1") && tags.contains(&"tag2"), "{tags:?}");
}
