use std::sync::Arc;

use pirs_rhai::ExtensionHost;
use serde_json::json;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load(name: &str, runner: Option<pirs_rhai::SubagentRunner>) -> Arc<ExtensionHost> {
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

fn user_msg(t: &str) -> pirs_ai::Message {
    pirs_ai::Message::user(t)
}

#[test]
fn dmail_folds_history_and_injects_note() {
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("dmail.rhai", None);
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();
    let listener = host.listener().unwrap();
    let tools = host.tools();
    let dmail = tools.iter().find(|t| t.name() == "dmail").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    transform(vec![user_msg("m1"), user_msg("m2")]);
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage::default()),
        tool_results: vec![],
    });
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage::default()),
        tool_results: vec![],
    });

    let out = rt
        .block_on(dmail.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"note": "that approach failed, go around", "turns_back": 1}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out.content[0].as_text().unwrap().contains("History folded"));

    let messages = vec![
        user_msg("m1"),
        user_msg("m2"),
        user_msg("m3"),
        user_msg("m4"),
        user_msg("m5"),
    ];
    let folded = transform(messages);
    let last = folded.last().unwrap();
    let pirs_ai::Message::User(u) = last else {
        panic!()
    };
    let pirs_ai::UserContent::Text(t) = &u.content else {
        panic!()
    };
    assert!(t.contains("dmail from your future self"));
    assert!(t.contains("that approach failed"));
    assert!(folded.len() < 6, "history folded: {}", folded.len());
}

#[test]
fn fork_includes_history_prefix() {
    let _g = ENV_LOCK.lock().unwrap();
    let captured: Arc<std::sync::Mutex<Vec<String>>> = Arc::new(std::sync::Mutex::new(Vec::new()));
    let captured2 = Arc::clone(&captured);
    let runner: pirs_rhai::SubagentRunner = Arc::new(move |task: String, _| {
        captured2.lock().unwrap().push(task);
        Ok("done".to_string())
    });
    let host = load("fork.rhai", Some(runner));
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();
    let tools = host.tools();
    let fork = tools.iter().find(|t| t.name() == "fork").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    transform(vec![user_msg("build a parser")]);
    rt.block_on(fork.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: json!({"task": "now optimize it"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }))
    .unwrap();

    let calls = captured.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].contains("build a parser"),
        "fork passes history: {}",
        calls[0]
    );
    assert!(calls[0].contains("now optimize it"));
}

#[test]
fn dirty_guard_commits_wip_before_edit() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-dg-{}", std::process::id()));
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
    std::fs::write(tmp.join("f.txt"), "user-wip\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = load("dirty-guard.rhai", None);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();

    assert!(
        before("1", "edit", &json!({"path": "f.txt"})).is_none(),
        "clean commit must pass"
    );

    let log = std::process::Command::new("git")
        .args(["log", "--oneline", "-2"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    let log = String::from_utf8_lossy(&log.stdout);
    assert!(log.contains("pre-ai-edit snapshot"), "{log}");

    let status = std::process::Command::new("git")
        .args(["status", "--porcelain"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    assert!(
        String::from_utf8_lossy(&status.stdout).trim().is_empty(),
        "wip committed"
    );
    std::env::set_current_dir(cwd).unwrap();
}

#[test]
fn safe_edit_rewrites_via_editor_prompt() {
    let _g = ENV_LOCK.lock().unwrap();
    let runner: pirs_rhai::SubagentRunner = Arc::new(|task: String, _| {
        assert!(task.contains("editor model") || task.contains("EDITOR"));
        Ok("fn add(a: i32, b: i32) -> i32 {\n    a + b\n}".to_string())
    });
    let host = load("safe_edit.rhai", Some(runner));
    let tools = host.tools();
    let safe_edit = tools.iter().find(|t| t.name() == "safe_edit").unwrap();

    let tmp = std::env::temp_dir().join(format!("pirs-se-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let f = tmp.join("a.rs");
    std::fs::write(&f, "fn add(a: i32, b: i32) -> i32 {\n    0\n}\n").unwrap();

    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(safe_edit.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: json!({"path": f.to_string_lossy(), "instruction": "return a + b"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out.content[0].as_text().unwrap().contains("rewrote"));
    let content = std::fs::read_to_string(&f).unwrap();
    assert_eq!(content, "fn add(a: i32, b: i32) -> i32 {\n    a + b\n}\n");
}

#[test]
fn btw_answers_without_history() {
    let _g = ENV_LOCK.lock().unwrap();
    let runner: pirs_rhai::SubagentRunner = Arc::new(|task: String, _| {
        if task.contains("side question") || task.contains("Side question") {
            Ok("42".to_string())
        } else {
            Ok("?".to_string())
        }
    });
    let host = load("btw.rhai", Some(runner));
    let hooks = host.hooks();
    hooks.transform_context.unwrap()(vec![user_msg("hello")]);
    let out = host.run_command("btw", "what is 6*7").unwrap();
    assert_eq!(out, "[btw] 42");
}

#[test]
fn checkpoints_snapshot_and_restore() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-cp-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    let f = tmp.join("f.txt");
    std::fs::write(&f, "v1\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let host = load("checkpoints.rhai", None);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    before("1", "edit", &json!({"path": f.to_string_lossy()}));

    let ckpt = tmp
        .join(".pirs/checkpoints")
        .join(f.to_string_lossy().replace("/", "_"));
    assert!(ckpt.exists(), "checkpoint written: {ckpt:?}");

    std::fs::write(&f, "v2\n").unwrap();
    let out = host.run_command("restore", &f.to_string_lossy()).unwrap();
    assert!(out.contains("restored"), "{out}");
    assert_eq!(std::fs::read_to_string(&f).unwrap(), "v1\n");
    std::env::set_current_dir(cwd).unwrap();
}
