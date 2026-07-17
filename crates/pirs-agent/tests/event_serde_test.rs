#[test]
fn agent_event_serializes_pi_style() {
    let ev = pirs_agent::AgentEvent::ToolExecutionStart {
        tool_call_id: "c1".into(),
        tool_name: "bash".into(),
        args: serde_json::json!({"command": "ls"}),
    };
    let v = serde_json::to_value(&ev).unwrap();
    assert_eq!(v["type"], "tool_execution_start");
    assert_eq!(v["toolCallId"], "c1");
    let ev2 = pirs_agent::AgentEvent::TurnStart;
    assert_eq!(serde_json::to_value(&ev2).unwrap()["type"], "turn_start");
}
