//! Policy packs: strict-plan (add-only denials), session-discipline (steering).

use std::sync::Arc;

use pirs_rhai::ExtensionHost;
use serde_json::json;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load(name: &str) -> Arc<ExtensionHost> {
    pirs_rhai::register_core_host_apis();
    let path = format!("{}/../../extensions/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut host = ExtensionHost::new();
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

#[test]
fn strict_plan_blocks_web_search_when_profile_plan() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("PIRS_AGENT_PROFILE", "plan");
    let host = load("strict-plan.rhai");
    let before = host.hooks().before_tool_call.expect("hook");
    let deny = before("1", "web_search", &json!({"query": "x"}));
    assert!(
        deny.as_ref().is_some_and(|s| s.contains("strict-plan")),
        "got {deny:?}"
    );
    // read is not in EXTRA_PLAN_BLOCK — pack must not invent denials for it.
    assert!(before("1", "read", &json!({"path": "a"})).is_none());
    std::env::remove_var("PIRS_AGENT_PROFILE");
}

#[test]
fn strict_plan_idle_when_profile_default() {
    let _g = ENV_LOCK.lock().unwrap();
    std::env::set_var("PIRS_AGENT_PROFILE", "default");
    std::env::remove_var("PIRS_STRICT_PLAN");
    let host = load("strict-plan.rhai");
    let before = host.hooks().before_tool_call.expect("hook");
    assert!(before("1", "web_search", &json!({"query": "x"})).is_none());
    std::env::remove_var("PIRS_AGENT_PROFILE");
}

#[test]
fn session_discipline_steers_todo_after_mutates() {
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("session-discipline.rhai");
    let hooks = host.hooks();
    let before = hooks.before_tool_call.expect("before");
    let steer = hooks.get_steering_messages.expect("steering");
    // Two mutates without todo.
    before("1", "edit", &json!({"path": "a"}));
    before("2", "write", &json!({"path": "b"}));
    let msgs = steer();
    assert!(
        msgs.iter().any(|m| {
            let t = match m {
                pirs_ai::Message::User(u) => match &u.content {
                    pirs_ai::UserContent::Text(s) => s.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            t.contains("todo")
        }),
        "expected todo steering, got {msgs:?}"
    );
}

#[test]
fn auto_checkpoint_calls_core_create_on_mutate() {
    let _g = ENV_LOCK.lock().unwrap();
    pirs_rhai::register_core_host_apis();
    let tmp = tempfile::tempdir().unwrap();
    let cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(tmp.path()).unwrap();
    std::fs::write(tmp.path().join("a.txt"), b"v1").unwrap();

    let host = load("auto-checkpoint.rhai");
    let after = host.hooks().after_tool_call.expect("after hook");
    let result = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "write".into(),
        content: vec![pirs_ai::ContentBlock::text("ok")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    let _ = after("1", "write", &result);
    let errs = host.drain_hook_errors();
    assert!(errs.is_empty(), "on_tool_result should not error: {errs:?}");

    let list = pirs_tools::list_checkpoints(tmp.path());
    assert!(
        !list.is_empty(),
        "auto-checkpoint pack must create a core checkpoint via host API; errs={errs:?} index={:?}",
        tmp.path().join(".pirs/checkpoints")
    );
    assert!(
        list[0]
            .copy_dir
            .as_ref()
            .is_some_and(|d| { std::path::Path::new(d).join("a.txt").is_file() }),
        "core checkpoint should snapshot a.txt: {:?}",
        list[0]
    );

    // Corrupt then restore via host API used by the pack command path.
    std::fs::write(tmp.path().join("a.txt"), b"dirty").unwrap();
    let msg = pirs_tools::restore_checkpoint(tmp.path(), Some(&list[0].id)).unwrap();
    assert!(msg.contains("restored"), "{msg}");
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
        "v1"
    );

    std::env::set_current_dir(cwd).unwrap();
}

#[test]
fn browser_cdp_workflow_steers_on_first_call() {
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("browser-cdp-workflow.rhai");
    let hooks = host.hooks();
    let before = hooks.before_tool_call.expect("before");
    let steer = hooks.get_steering_messages.expect("steering");
    before("1", "browser_cdp", &json!({"action": "connect"}));
    let msgs = steer();
    assert!(
        msgs.iter().any(|m| {
            let t = match m {
                pirs_ai::Message::User(u) => match &u.content {
                    pirs_ai::UserContent::Text(s) => s.clone(),
                    _ => String::new(),
                },
                _ => String::new(),
            };
            t.contains("browser_cdp") || t.contains("PIRS_BROWSER_CDP")
        }),
        "expected cdp recipe steer, got {msgs:?}"
    );
}
