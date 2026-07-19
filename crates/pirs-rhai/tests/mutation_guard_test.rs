use std::sync::Arc;

use pirs_rhai::ExtensionHost;

fn load() -> Arc<ExtensionHost> {
    // Mock the graph query: only lib.rs has reaching tests.
    pirs_rhai::register_query_fn("graph_affected_tests", |path| {
        if path.ends_with("lib.rs") {
            vec!["test_parse".to_string(), "test_helper".to_string()]
        } else {
            vec![]
        }
    });
    let path = format!(
        "{}/../../examples/extensions/mutation-guard.rhai",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut host = ExtensionHost::new();
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

fn tool_result(name: &str, text: &str) -> pirs_ai::ToolResultMessage {
    pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: name.into(),
        content: vec![pirs_ai::ContentBlock::text(text)],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    }
}

#[test]
fn build_plan_scopes_tests_via_graph() {
    let host = load();
    let arg = rhai::Dynamic::from(vec![rhai::Dynamic::from("crates/x/src/lib.rs")]);
    let d = host
        .call_extension_for_test(0, "build_plan", (arg,))
        .unwrap();
    let m = d.cast::<rhai::Map>();
    let cmd = m.get("cmd").unwrap().clone().cast::<String>();
    assert!(cmd.contains("cargo mutants"), "{cmd}");
    assert!(cmd.contains("-f crates/x/src/lib.rs"), "{cmd}");
    assert!(
        cmd.contains("-- test_parse test_helper") || cmd.contains("-- test_helper test_parse"),
        "{cmd}"
    );
    assert!(m.get("scoped").unwrap().as_bool().unwrap());
    assert_eq!(
        m.get("untested")
            .unwrap()
            .clone()
            .cast::<rhai::Array>()
            .len(),
        0
    );
}

#[test]
fn build_plan_flags_files_with_no_reaching_tests() {
    let host = load();
    let arg = rhai::Dynamic::from(vec![rhai::Dynamic::from("src/other.rs")]);
    let d = host
        .call_extension_for_test(0, "build_plan", (arg,))
        .unwrap();
    let m = d.cast::<rhai::Map>();
    let cmd = m.get("cmd").unwrap().clone().cast::<String>();
    // No reaching tests -> no `--` test filter, and the file is surfaced as
    // untested so the pack can flag it rather than silently pass.
    assert!(!cmd.contains(" -- "), "{cmd}");
    assert!(!m.get("scoped").unwrap().as_bool().unwrap());
    let untested = m.get("untested").unwrap().clone().cast::<rhai::Array>();
    assert_eq!(untested.len(), 1);
    assert_eq!(untested[0].clone().cast::<String>(), "src/other.rs");
}

#[test]
fn on_tool_result_records_only_source_files() {
    let host = load();
    let after = host.hooks().after_tool_call.expect("after hook");

    // Source edit -> recorded; on_tool_result returns no patch.
    assert!(after(
        "1",
        "write",
        &tool_result(
            "write",
            "Successfully wrote 10 bytes to crates/x/src/lib.rs"
        )
    )
    .is_none());
    // Test-file edit -> filtered out by is_source_rs.
    assert!(after(
        "2",
        "edit",
        &tool_result(
            "edit",
            "Successfully replaced 1 block(s) in crates/x/tests/foo_test.rs"
        )
    )
    .is_none());

    let files = host
        .call_extension_for_test(0, "files_list", ())
        .unwrap()
        .cast::<rhai::Array>();
    assert_eq!(files.len(), 1);
    assert_eq!(files[0].clone().cast::<String>(), "crates/x/src/lib.rs");
}
