//! Approval-gate / profile installation helpers for agent hooks.

/// Installs the approval gate as the before-tool hook when nothing else claimed
/// the slot. Yolo mode explicitly waives this install (interactive approval);
/// safety-profile denials under yolo are chained separately via
/// [`chain_gate_with_extensions`] / [`install_profile_under_yolo_if_needed`].
pub fn install_gate_if_absent(
    hooks: &mut pirs_agent::Hooks,
    gate_hook: &Option<pirs_agent::events::BeforeToolCallHook>,
    approval: &str,
) {
    let yolo = crate::approval::ApprovalMode::parse(approval) == Some(crate::approval::ApprovalMode::Yolo);
    if !yolo && hooks.before_tool_call.is_none() {
        hooks.before_tool_call = gate_hook.clone();
    }
}

/// Under yolo, still enforce non-default `--agent-profile` hard denials when no
/// before_tool hook was installed (e.g. `--no-extensions`).
pub fn install_profile_under_yolo_if_needed(
    hooks: &mut pirs_agent::Hooks,
    gate_hook: &Option<pirs_agent::events::BeforeToolCallHook>,
    approval: &str,
    safety: pirs_tools::SafetyProfile,
) {
    let yolo = crate::approval::ApprovalMode::parse(approval) == Some(crate::approval::ApprovalMode::Yolo);
    if yolo
        && safety != pirs_tools::SafetyProfile::Default
        && hooks.before_tool_call.is_none()
    {
        hooks.before_tool_call = gate_hook.clone();
    }
}

/// Chain approval/profile gate with extension before_tool hooks.
///
/// Pure yolo still keeps `gate_hook` when present — production always folds the
/// live permission ladder into `gate_hook`, and yolo must not drop that ladder
/// (only interactive approval prompts are waived via ApprovalMode::Yolo).
/// Non-default safety profiles still run hard denials under yolo.
pub fn chain_gate_with_extensions(
    gate_hook: Option<pirs_agent::events::BeforeToolCallHook>,
    ext_before: Option<pirs_agent::events::BeforeToolCallHook>,
    yolo: bool,
    safety: pirs_tools::SafetyProfile,
) -> Option<pirs_agent::events::BeforeToolCallHook> {
    let _ = (yolo, safety); // gate_hook already encodes profile/permission; always chain.
    // Gate first (permission ladder / profile denials / optional prompts), then ext.
    pirs_agent::Hooks::chain_before(gate_hook, ext_before)
}

pub fn summarize_args(tool: &str, args: &serde_json::Value) -> String {
    let key = match tool {
        "bash" => "command",
        "read" | "write" | "edit" => "path",
        "grep" | "find" => "pattern",
        _ => "",
    };
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| {
            let s = s.replace('\n', " ");
            if s.chars().count() > 80 {
                format!("{}...", s.chars().take(80).collect::<String>())
            } else {
                s
            }
        })
        .unwrap_or_default()
}
