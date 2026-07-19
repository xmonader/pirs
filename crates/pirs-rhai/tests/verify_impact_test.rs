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

fn plan(host: &Arc<ExtensionHost>, path: &str) -> rhai::Map {
    let arg = rhai::Dynamic::from(path.to_string());
    host.call_extension_for_test(0, "impact_plan", (arg,))
        .unwrap()
        .cast::<rhai::Map>()
}

#[test]
fn impact_plan_scopes_to_graph_tests() {
    let host = load_pack();
    let m = plan(&host, "src/lib.rs");
    let cmd = m.get("cmd").unwrap().clone().cast::<String>();
    assert!(m.get("scoped").unwrap().as_bool().unwrap());
    assert!(
        cmd.contains("cargo test -- test_parse_config test_helper")
            || cmd.contains("cargo test -- test_helper test_parse_config"),
        "{cmd}"
    );
}

#[test]
fn impact_plan_fails_open_to_full_suite_when_no_tests() {
    // The graph finds no tests reaching src/other.rs. The gate must run the
    // FULL suite (fail open), not silently skip — the old behavior returned
    // nothing here, letting an unverified edit look green.
    let host = load_pack();
    let m = plan(&host, "src/other.rs");
    assert!(!m.get("scoped").unwrap().as_bool().unwrap());
    assert_eq!(m.get("cmd").unwrap().clone().cast::<String>(), "cargo test");
}

#[test]
fn non_edit_and_error_results_are_ignored() {
    let host = load_pack();
    let after = host.hooks().after_tool_call.expect("after hook");
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
}
