use std::sync::{Arc, Mutex};

use pirs_agent::events::BeforeToolCallHook;
use pirs_rhai::ExtensionHost;
use serde_json::json;

/// A policy hook that spawns a sub-agent whose own policy evaluation lands on
/// the SAME extension host. The extension lock is held by the outer hook while
/// the sub-agent runs, so the inner evaluation hits a busy lock. With blocking
/// locks this deadlocks; the guard must fail closed instead.
const SCRIPT: &str = r#"
fn on_tool_call(id, name, args) {
    if name == "spawn_first" {
        let answer = run_subagent("check policy");
        if answer.contains("blocked") {
            return #{ block: true, reason: "sub-agent was policy-blocked" };
        }
    }
    ()
}
"#;

#[test]
fn reentrant_policy_hook_fails_closed_not_deadlock() {
    let mut host = ExtensionHost::new();

    // The runner re-enters the host's before-hook, exactly like a sub-agent
    // whose policy hooks point at this same ExtensionHost. The runner must be
    // set before load: `run_subagent` is registered per-engine at load time.
    let hook_slot: Arc<Mutex<Option<BeforeToolCallHook>>> = Arc::new(Mutex::new(None));
    let slot2 = Arc::clone(&hook_slot);
    host.set_subagent_runner(Arc::new(move |_task, _model| {
        let before = slot2.lock().unwrap().clone().unwrap();
        let blocked = before("sub", "bash", &json!({"command": "ls"}));
        Ok(if blocked.is_some() {
            "sub-agent blocked".to_string()
        } else {
            "sub-agent allowed".to_string()
        })
    }));
    host.load_source(SCRIPT, "reentrant.rhai".into()).unwrap();

    let host = Arc::new(host);
    let before = host.hooks().before_tool_call.expect("hook registered");
    *hook_slot.lock().unwrap() = Some(before.clone());

    let verdict = before("1", "spawn_first", &json!({}));
    assert_eq!(
        verdict.as_deref(),
        Some("sub-agent was policy-blocked"),
        "re-entrant policy evaluation must fail closed, got: {verdict:?}"
    );
}
