use super::*;

#[test]
fn short_c_flag_is_session_cwd() {
    use clap::Parser;
    // Bench muscle memory: -C DIR must set cwd the same as --cwd DIR.
    // (Previously -C was ignored and agents edited the suite tree.)
    let with_short =
        Cli::try_parse_from(["pirs", "-C", "/tmp/bench-ws", "hello"]).expect("short -C");
    let with_long =
        Cli::try_parse_from(["pirs", "--cwd", "/tmp/bench-ws", "hello"]).expect("long --cwd");
    assert_eq!(
        with_short.cwd.as_deref(),
        Some(std::path::Path::new("/tmp/bench-ws"))
    );
    assert_eq!(with_short.cwd, with_long.cwd);
    assert_eq!(with_short.prompt, vec!["hello".to_string()]);
}

#[test]
fn serve_token_is_random_and_long() {
    let a = generate_serve_token();
    let b = generate_serve_token();
    assert_eq!(a.len(), 64);
    assert!(a.chars().all(|c| c.is_ascii_hexdigit()));
    assert_ne!(a, b, "tokens must not be predictable/repeating");
}
use std::sync::Arc;

fn gate() -> Option<pirs_agent::events::BeforeToolCallHook> {
    Some(Arc::new(|_, name, _| {
        if name == "danger" {
            Some("blocked by gate".to_string())
        } else {
            None
        }
    }))
}

#[test]
fn gate_installed_when_hooks_empty() {
    // --approval ask --no-extensions: previously no gate was installed.
    let mut hooks = pirs_agent::Hooks::default();
    install_gate_if_absent(&mut hooks, &gate(), "ask");
    let before = hooks.before_tool_call.expect("gate must be installed");
    assert_eq!(
        before("1", "danger", &serde_json::json!({})).as_deref(),
        Some("blocked by gate")
    );
}

#[test]
fn gate_not_installed_in_yolo() {
    let mut hooks = pirs_agent::Hooks::default();
    install_gate_if_absent(&mut hooks, &gate(), "yolo");
    assert!(hooks.before_tool_call.is_none());
}

#[test]
fn existing_hook_not_overwritten() {
    let mut hooks = pirs_agent::Hooks {
        before_tool_call: Some(Arc::new(|_, _, _| Some("ext".to_string()))),
        ..Default::default()
    };
    install_gate_if_absent(&mut hooks, &gate(), "ask");
    let before = hooks.before_tool_call.unwrap();
    assert_eq!(
        before("1", "x", &serde_json::json!({})).as_deref(),
        Some("ext")
    );
}

#[test]
fn yolo_with_plan_profile_installs_denials_without_extensions() {
    let mut hooks = pirs_agent::Hooks::default();
    install_gate_if_absent(&mut hooks, &gate(), "yolo");
    assert!(hooks.before_tool_call.is_none());
    install_profile_under_yolo_if_needed(
        &mut hooks,
        &gate(),
        "yolo",
        pirs_tools::SafetyProfile::Plan,
    );
    let before = hooks.before_tool_call.expect("profile under yolo");
    assert_eq!(
        before("1", "danger", &serde_json::json!({})).as_deref(),
        Some("blocked by gate")
    );
}

#[test]
fn yolo_with_plan_chains_gate_before_extension() {
    let ext: pirs_agent::events::BeforeToolCallHook = Arc::new(|_, _, _| Some("ext-deny".into()));
    let chained =
        chain_gate_with_extensions(gate(), Some(ext), true, pirs_tools::SafetyProfile::Plan)
            .expect("chained");
    // Gate runs first: danger blocked by gate, not ext.
    assert_eq!(
        chained("1", "danger", &serde_json::json!({})).as_deref(),
        Some("blocked by gate")
    );
    // Non-danger falls through to extension.
    assert_eq!(
        chained("1", "read", &serde_json::json!({})).as_deref(),
        Some("ext-deny")
    );
}

#[test]
fn pure_yolo_still_chains_gate_hook_then_extension() {
    // Production pure-yolo gate_hook is the live permission ladder (no prompts).
    // Yolo must not drop that ladder — only interactive approval is waived.
    let ext: pirs_agent::events::BeforeToolCallHook = Arc::new(|_, _, _| Some("ext-only".into()));
    let chained =
        chain_gate_with_extensions(gate(), Some(ext), true, pirs_tools::SafetyProfile::Default)
            .expect("chained");
    // Gate runs first.
    assert_eq!(
        chained("1", "danger", &serde_json::json!({})).as_deref(),
        Some("blocked by gate")
    );
    // Non-danger falls through to extension.
    assert_eq!(
        chained("1", "read", &serde_json::json!({})).as_deref(),
        Some("ext-only")
    );
}

#[test]
fn pure_yolo_with_permission_ladder_denies_bash_under_read_only() {
    pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::ReadOnly);
    let perm = Some(pirs_tools::live_permission_hook());
    let chained = chain_gate_with_extensions(perm, None, true, pirs_tools::SafetyProfile::Default)
        .expect("perm under yolo");
    assert!(chained("1", "bash", &serde_json::json!({"command": "ls"})).is_some());
    assert!(chained("1", "read", &serde_json::json!({"path": "a"})).is_none());
    pirs_tools::set_live_permission_mode(pirs_tools::PermissionMode::DangerFullAccess);
}

#[test]
fn chain_with_before_only_ext_still_returns_gate_under_plan() {
    // Packs like strict-plan only register on_tool_call (before), no after.
    let ext: pirs_agent::events::BeforeToolCallHook = Arc::new(|_, name, _| {
        if name == "web_search" {
            Some("strict".into())
        } else {
            None
        }
    });
    let chained =
        chain_gate_with_extensions(gate(), Some(ext), false, pirs_tools::SafetyProfile::Plan)
            .expect("before-only chain");
    assert_eq!(
        chained("1", "danger", &serde_json::json!({})).as_deref(),
        Some("blocked by gate")
    );
    assert_eq!(
        chained("1", "web_search", &serde_json::json!({})).as_deref(),
        Some("strict")
    );
}

/// Production sources after the main/repl/turn split (scan all of them).
fn production_bin_src() -> String {
    concat!(
        include_str!("main.rs"),
        include_str!("repl.rs"),
        include_str!("turn.rs"),
        include_str!("cli.rs"),
        include_str!("printer.rs"),
        include_str!("gates.rs"),
        include_str!("login.rs"),
    )
    .to_string()
}

/// Production one-shot / REPL exits must thread ReportPins (no hard-coded empty pins).
#[test]
fn production_exit_paths_use_report_pins_not_hardcoded_none() {
    let prod = production_bin_src();
    assert!(
        prod.contains("ReportPins::from_cli"),
        "main must resolve ReportPins once from CLI"
    );
    assert!(
        prod.contains("print_usage_end"),
        "one-shot exit must call crate::session_stats::print_usage_end"
    );
    assert!(
        prod.contains("print_session_stats_pins"),
        "REPL session-end and /usage must call print_session_stats_pins"
    );
    // No production hardcode of empty pins at the classic print_session_stats sites.
    assert!(
        !prod.contains("&agent.model,\n        None,\n        None,"),
        "REPL must not hardcode plan_model=None strategy=None at print sites"
    );
    assert!(
        !prod.contains("&agent.model,\n                None,\n                None,"),
        "/usage must not hardcode empty pins"
    );
}

/// Residual: strategy path must emit phase.end (not only phase.start).
#[test]
fn strategy_path_records_phase_end() {
    let prod = production_bin_src();
    assert!(
        prod.contains("record_phase_start"),
        "strategy path must record phase.start"
    );
    assert!(
        prod.contains("record_phase_end"),
        "strategy path must record phase.end (pair with start)"
    );
    assert!(
        prod.contains("crate::discovery::skills_prompt_block")
            && prod.contains("crate::discovery::discover_skills"),
        "main must use discovery skill helpers (not dead re-exports)"
    );
    assert!(
        prod.contains("fc.path.display()"),
        "/help must surface FileCommand.path"
    );
    assert!(
        prod.contains("d.kind"),
        "replay CLI must print Divergence.kind"
    );
}

/// All three surfaces (one-shot, REPL, TUI) share pin + role-split APIs.
#[test]
fn all_exit_surfaces_use_shared_report_apis() {
    let main_prod = production_bin_src();
    let tui_prod = concat!(
        include_str!("tui/mod.rs"),
        include_str!("tui/app.rs"),
        include_str!("tui/slash_exec.rs"),
    );
    assert!(main_prod.contains("print_usage_end") && main_prod.contains("report_pins"));
    assert!(main_prod.contains("print_session_stats_pins"));
    assert!(tui_prod.contains("print_session_stats_pins"));
    assert!(tui_prod.contains("format_session_stats_pins"));
    assert!(tui_prod.contains("app.report_pins()"));
    // Shared role-split lives only in session_stats (single template).
    let stats = include_str!("session_stats.rs");
    let stats_prod = stats.split("#[cfg(test)]\nmod tests {").next().unwrap();
    assert_eq!(
        stats_prod.matches("\"  by role\"").count(),
        1,
        "single by-role template"
    );
}
