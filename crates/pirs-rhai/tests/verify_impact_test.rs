use std::sync::Arc;

use pirs_rhai::ExtensionHost;

fn load_pack() -> Arc<ExtensionHost> {
    pirs_rhai::register_query_fn("graph_affected_tests", |path| {
        if path.ends_with("lib.rs") {
            vec!["test_parse_config".to_string(), "test_helper".to_string()]
        } else {
            vec![]
        }
    });
    let path = format!(
        "{}/../../examples/extensions/verify-impact.rhai",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut host = ExtensionHost::new();
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

#[test]
fn impact_gate_records_command_and_extracts_path() {
    let host = load_pack();
    let hooks = host.hooks();
    let after = hooks.after_tool_call.expect("after hook");

    // write tool: path parsed from result text ("... to <path>").
    let wr = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "write".into(),
        content: vec![pirs_ai::ContentBlock::text(
            "Successfully wrote 42 bytes to src/lib.rs",
        )],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    let patch = after("1", "write", &wr);
    // Not dry_run: the pack actually ran cargo. We only assert it produced a
    // verdict patch referencing the impacted tests command.
    let text = patch
        .and_then(|p| p.content)
        .and_then(|c| c[0].as_text().map(|s| s.to_string()))
        .unwrap_or_default();
    assert!(
        text.contains("cargo test -- test_parse_config test_helper")
            || text.contains("cargo test -- test_helper test_parse_config"),
        "{text}"
    );
    assert!(text.contains("[impact"), "{text}");

    // Non-edit tool: ignored (no patch).
    let other = pirs_ai::ToolResultMessage {
        tool_call_id: "2".into(),
        tool_name: "bash".into(),
        content: vec![pirs_ai::ContentBlock::text("ok")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    assert!(after("2", "bash", &other).is_none());

    // Edit of a file with no impacted tests: no patch.
    let none = pirs_ai::ToolResultMessage {
        tool_call_id: "3".into(),
        tool_name: "edit".into(),
        content: vec![pirs_ai::ContentBlock::text(
            "Successfully replaced 1 block(s) in src/other.rs",
        )],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    assert!(after("3", "edit", &none).is_none());
}
