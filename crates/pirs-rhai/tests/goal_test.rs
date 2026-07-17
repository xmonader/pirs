use std::sync::Arc;
use pirs_rhai::ExtensionHost;
use serde_json::json;

fn host() -> Arc<ExtensionHost> {
    let path = format!("{}/../../examples/extensions/goal.rhai", env!("CARGO_MANIFEST_DIR"));
    let mut h = ExtensionHost::new();
    h.load_source(&std::fs::read_to_string(&path).unwrap(), path).unwrap();
    Arc::new(h)
}

#[test]
fn goal_lifecycle() {
    let tmp = std::env::temp_dir().join(format!("pirs-goal-{}", std::process::id()));
    std::env::set_var("HOME", &tmp);
    let host = host();
    let tools = host.tools();
    let set_goal = tools.iter().find(|t| t.name() == "set_goal").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(set_goal.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: json!({"goal": "ship the Anthropic provider"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    })).unwrap();
    assert!(out.content[0].as_text().unwrap().contains("Goal set"));

    let hooks = host.hooks();
    let transform = hooks.transform_context.as_ref().unwrap();
    let msgs = transform(vec![pirs_ai::Message::user("work")]);
    assert!(msgs.iter().any(|m| matches!(
        m,
        pirs_ai::Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("[SESSION GOAL]") && t.contains("ship the Anthropic provider"))
    )), "goal pinned");

    let follow = hooks.get_follow_up_messages.as_ref().unwrap();
    let msgs = follow();
    assert_eq!(msgs.len(), 1, "verification fires once");
    assert!(follow().is_empty());

    assert!(host.run_command("goal", "").unwrap().contains("ship the Anthropic provider"));
    let cwd = std::env::current_dir().unwrap();
    let persisted = cwd.join(".pirs/goal.md");
    assert!(persisted.exists());
    let _ = std::fs::remove_file(&persisted);
}
