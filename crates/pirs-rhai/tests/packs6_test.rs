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

fn tr(name: &str, text: &str, is_error: bool) -> pirs_ai::ToolResultMessage {
    pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: name.into(),
        content: vec![pirs_ai::ContentBlock::text(text)],
        details: None,
        is_error,
        terminate: false,
        timestamp: 0,
    }
}

fn user_msg(t: &str) -> pirs_ai::Message {
    pirs_ai::Message::user(t)
}

#[test]
fn review_gate_blocks_critical_and_releases_on_sound() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-rg-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::process::Command::new("git").args(["init", "-q"]).current_dir(&tmp).output().unwrap();
    std::fs::write(tmp.join("f.txt"), "v1\n").unwrap();
    std::process::Command::new("git").args(["add", "-A"]).current_dir(&tmp).output().unwrap();
    std::process::Command::new("git")
        .args(["-c", "user.email=t@t", "-c", "user.name=t", "commit", "-qm", "init"])
        .current_dir(&tmp)
        .output()
        .unwrap();
    std::fs::write(tmp.join("f.txt"), "v2\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let critical: pirs_rhai::SubagentRunner = Arc::new(|_, _| {
        Ok("CRITICAL\n- changes the wrong thing".to_string())
    });
    let host = load("review-gate.rhai", Some(critical));
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();
    let follow = hooks.get_follow_up_messages.unwrap();
    let stop = hooks.should_stop_after_turn.unwrap();
    let after = hooks.after_tool_call.unwrap();

    transform(vec![user_msg("fix the bug")]);
    after("1", "edit", &tr("edit", "ok", false));

    let msgs = follow();
    assert_eq!(msgs.len(), 1, "blocked message injected: {msgs:?}");
    let pirs_ai::Message::User(u) = &msgs[0] else { panic!() };
    let pirs_ai::UserContent::Text(t) = &u.content else { panic!() };
    assert!(t.contains("REVIEW BLOCKED"));
    assert!(stop(&pirs_ai::Context::default()), "should_stop must hold while blocked");

    std::env::set_current_dir(cwd).unwrap();
}

#[test]
fn review_gate_sound_releases() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-rg2-{}", std::process::id()));
    std::fs::create_dir_all(&tmp).unwrap();
    std::process::Command::new("git").args(["init", "-q"]).current_dir(&tmp).output().unwrap();
    std::fs::write(tmp.join("f.txt"), "v2\n").unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(&tmp).unwrap();

    let sound: pirs_rhai::SubagentRunner = Arc::new(|_, _| Ok("SOUND".to_string()));
    let host = load("review-gate.rhai", Some(sound));
    let hooks = host.hooks();
    let follow = hooks.get_follow_up_messages.unwrap();
    let stop = hooks.should_stop_after_turn.unwrap();
    let after = hooks.after_tool_call.unwrap();

    after("1", "edit", &tr("edit", "ok", false));
    assert!(follow().is_empty());
    assert!(!stop(&pirs_ai::Context::default()));
    std::env::set_current_dir(cwd).unwrap();
}

#[test]
fn verify_guard_flags_zero_tests() {
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("verify-guard.rhai", None);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let after = hooks.after_tool_call.unwrap();
    let steering = hooks.get_steering_messages.unwrap();

    after("1", "edit", &tr("edit", "ok", false));
    before("2", "bash", &json!({"command": "pytest -q"}));
    after("2", "bash", &tr("bash", "collected 0 items\n", false));
    let msgs = steering();
    assert_eq!(msgs.len(), 1);
    let pirs_ai::Message::User(u) = &msgs[0] else { panic!() };
    let pirs_ai::UserContent::Text(t) = &u.content else { panic!() };
    assert!(t.contains("ZERO passing tests"));
}

#[test]
fn verify_guard_accepts_real_tests() {
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("verify-guard.rhai", None);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let after = hooks.after_tool_call.unwrap();
    let steering = hooks.get_steering_messages.unwrap();

    after("1", "edit", &tr("edit", "ok", false));
    before("2", "bash", &json!({"command": "pytest -q"}));
    after("2", "bash", &tr("bash", "9 passed in 0.08s\n", false));
    assert!(steering().is_empty());
}

#[test]
fn spend_caps_stop_at_budget() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-spend-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let host = load("spend-caps.rhai", None);
    let listener = host.listener().unwrap();
    let hooks = host.hooks();
    let stop = hooks.should_stop_after_turn.unwrap();

    let ev = |tokens: i64| pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage {
            usage: pirs_ai::Usage {
                input: tokens as u64,
                output: tokens as u64,
                total_tokens: (tokens * 2) as u64,
                ..Default::default()
            },
            ..Default::default()
        }),
        tool_results: vec![],
    };
    // 100k in + 100k out per event = $0.4; cap is $2.0 daily
    for _ in 0..2 {
        listener(ev(100_000));
    }
    assert!(!stop(&pirs_ai::Context::default()), "under cap ($0.8)");
    for _ in 0..4 {
        listener(ev(100_000));
    }
    assert!(stop(&pirs_ai::Context::default()), "cap reached ($2.4)");
    let spend = std::fs::read_to_string(tmp.join(".pirs/spend.json")).unwrap();
    assert!(spend.contains("2.4"), "persisted spend: {spend}");
}

#[test]
fn runs_records_and_recovers_interrupted() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = std::env::temp_dir().join(format!("pirs-runs-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let host = load("runs.rhai", None);
    let listener = host.listener().unwrap();
    let hooks = host.hooks();
    let steering = hooks.get_steering_messages.unwrap();

    listener(pirs_agent::AgentEvent::AgentStart);
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage {
            content: vec![pirs_ai::ContentBlock::text("working")],
            ..Default::default()
        }),
        tool_results: vec![],
    });

    let msgs = steering();
    assert_eq!(msgs.len(), 1, "interrupted run should be reported: {msgs:?}");
    let pirs_ai::Message::User(u) = &msgs[0] else { panic!() };
    let pirs_ai::UserContent::Text(t) = &u.content else { panic!() };
    assert!(t.contains("interrupted"));
}

#[test]
fn sha256_hex_works() {
    let _g = ENV_LOCK.lock().unwrap();
    let mut host = ExtensionHost::new();
    host.load_source(r#"fn h(x) { sha256_hex(x) } register_tool("h", "h", #{ type: "object", properties: #{ x: #{ type: "string" } }, required: ["x"] }); fn tool_h(args) { h(args.x) }"#, "h.rhai".into()).unwrap();
    let host = Arc::new(host);
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(host.tools()[0].execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: json!({"x": "pirs"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    })).unwrap();
    let digest = out.content[0].as_text().unwrap();
    assert_eq!(digest.len(), 64);
    assert!(digest.chars().all(|c| c.is_ascii_hexdigit()));
}
