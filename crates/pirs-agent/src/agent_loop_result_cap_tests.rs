use super::*;
use pirs_ai::ContentBlock;

#[test]
fn cap_chars_tail_keeps_end() {
    let s: String = (0..100).map(|i| format!("{i}")).collect();
    let t = cap_chars_tail(&s, 10);
    assert_eq!(t.chars().count(), 10);
    assert!(s.ends_with(&t) || t.chars().all(|c| s.contains(c)));
}

#[test]
fn apply_model_result_cap_spills_ui_text() {
    let big = "x".repeat(MODEL_MAX_TOOL_RESULT_CHARS + 500);
    let mut result = ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "t".into(),
        content: vec![ContentBlock::text(big.clone())],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    apply_model_result_cap(&mut result);
    assert!(result.model_text().chars().count() <= MODEL_MAX_TOOL_RESULT_CHARS + 120);
    assert!(result.model_text().contains("truncated"));
    let ui = result
        .details
        .as_ref()
        .and_then(|d| d.get("uiText"))
        .and_then(|v| v.as_str())
        .unwrap();
    assert_eq!(ui.len(), big.len());
}

#[test]
fn error_result_kind_tags_and_truncates() {
    let big = "e".repeat(MODEL_MAX_ERROR_CHARS + 200);
    let r = error_result_kind("id", "bash", &big, "exec");
    assert!(r.is_error);
    assert!(r.model_text().chars().count() <= MODEL_MAX_ERROR_CHARS + 40);
    assert_eq!(
        r.details
            .as_ref()
            .and_then(|d| d.get("errorKind"))
            .and_then(|v| v.as_str()),
        Some("exec")
    );
}

#[test]
fn finalize_result_caps_ok_output() {
    let big = "z".repeat(MODEL_MAX_TOOL_RESULT_CHARS + 1000);
    let out = crate::tool::ToolOutput::text(big);
    let r = finalize_result("c1", "mcp_tool", Ok(out), &Hooks::default());
    assert!(!r.is_error);
    assert!(r.model_text().chars().count() <= MODEL_MAX_TOOL_RESULT_CHARS + 120);
    assert!(r
        .details
        .as_ref()
        .and_then(|d| d.get("uiText"))
        .is_some());
}

#[test]
fn transient_tool_error_classifier() {
    assert!(is_transient_tool_error(&anyhow::anyhow!("connection reset by peer")));
    assert!(is_transient_tool_error(&anyhow::anyhow!("request timed out")));
    // Bare "503" / "try again" no longer trigger retries (mutating tool hazard).
    assert!(!is_transient_tool_error(&anyhow::anyhow!("HTTP 503")));
    assert!(!is_transient_tool_error(&anyhow::anyhow!("please try again later")));
    assert!(!is_transient_tool_error(&anyhow::anyhow!("file not found")));
    assert!(!is_transient_tool_error(&anyhow::anyhow!("Invalid arguments")));
}

#[test]
fn idempotent_tool_retry_gate() {
    assert!(is_idempotent_tool("read"));
    assert!(is_idempotent_tool("grep"));
    assert!(is_idempotent_tool("web_fetch"));
    assert!(is_idempotent_tool("mcp_search"));
    assert!(!is_idempotent_tool("bash"));
    assert!(!is_idempotent_tool("write"));
    assert!(!is_idempotent_tool("edit"));
    assert!(!is_idempotent_tool("mcp_call"));
    assert!(!is_idempotent_tool("mcp_enable"));
}
