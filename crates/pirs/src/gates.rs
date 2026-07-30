//! Approval-gate / profile installation helpers for agent hooks.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use pirs_agent::{Hooks, ToolResultPatch};
use pirs_ai::ContentBlock;

/// Installs the approval gate as the before-tool hook when nothing else claimed
/// the slot. Yolo mode explicitly waives this install (interactive approval);
/// safety-profile denials under yolo are chained separately via
/// [`chain_gate_with_extensions`] / [`install_profile_under_yolo_if_needed`].
pub fn install_gate_if_absent(
    hooks: &mut pirs_agent::Hooks,
    gate_hook: &Option<pirs_agent::events::BeforeToolCallHook>,
    approval: &str,
) {
    let yolo =
        crate::approval::ApprovalMode::parse(approval) == Some(crate::approval::ApprovalMode::Yolo);
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
    let yolo =
        crate::approval::ApprovalMode::parse(approval) == Some(crate::approval::ApprovalMode::Yolo);
    if yolo && safety != pirs_tools::SafetyProfile::Default && hooks.before_tool_call.is_none() {
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

/// Min seconds between post-edit verify runs (debounce multi-file edit bursts).
const POST_EDIT_VERIFY_DEBOUNCE_SECS: u64 = 25;

/// Wrap `after_tool_call` so successful file mutations always get a structured
/// project check (typecheck → lint → test). Opt out via `PIRS_POST_EDIT_VERIFY=0`.
pub fn install_post_edit_verify_hook(hooks: &mut Hooks, cwd: &Path) {
    if !pirs_tools::post_edit_verify_enabled() {
        eprintln!("[post_edit_verify: disabled via PIRS_POST_EDIT_VERIFY]");
        return;
    }
    let cwd = cwd.to_path_buf();
    let inner = hooks.after_tool_call.take();
    let last_run_ms = Arc::new(AtomicU64::new(0));
    hooks.after_tool_call = Some(Arc::new(move |id, name, result| {
        // Run existing after-hooks first (graph blast radius, rhai packs).
        let mut content = result.content.clone();
        let mut details = result.details.clone();
        let mut is_error = None;
        let mut terminate = None;
        let mut patched = false;
        if let Some(ref f) = inner {
            if let Some(p) = f(id, name, result) {
                if let Some(c) = p.content {
                    content = c;
                    patched = true;
                }
                if p.details.is_some() {
                    details = p.details;
                    patched = true;
                }
                if p.is_error.is_some() {
                    is_error = p.is_error;
                    patched = true;
                }
                if p.terminate.is_some() {
                    terminate = p.terminate;
                    patched = true;
                }
            }
        }

        let errored = is_error.unwrap_or(result.is_error);
        if pirs_tools::is_post_edit_verify_tool(name) && !errored {
            if should_debounce_post_edit(&last_run_ms) {
                content.push(ContentBlock::text(
                    "VERIFY DEBOUNCE: skipped (ran recently; set PIRS_POST_EDIT_VERIFY=0 to disable)"
                        .to_string(),
                ));
                patched = true;
            } else {
                let edited = extract_edit_path(result, &cwd);
                let started = Instant::now();
                let outcome = pirs_tools::post_edit_verify_for_path(&cwd, edited.as_deref(), 90);
                mark_post_edit_run(&last_run_ms);
                let note = pirs_tools::format_verify_for_tool_result(&outcome);
                eprintln!(
                    "[post_edit_verify: {} in {:.1}s]",
                    if outcome.skipped {
                        "skip"
                    } else if outcome.passed {
                        "pass"
                    } else {
                        "fail"
                    },
                    started.elapsed().as_secs_f32()
                );
                content.push(ContentBlock::text(note));
                // Structured details for packs / telemetry.
                let mut det = details.unwrap_or_else(|| serde_json::json!({}));
                if let Some(obj) = det.as_object_mut() {
                    obj.insert(
                        "post_edit_verify".into(),
                        serde_json::json!({
                            "passed": outcome.passed,
                            "skipped": outcome.skipped,
                            "action": outcome.action,
                            "command": outcome.command,
                            "exit_code": outcome.exit_code,
                            "ecosystem": outcome.ecosystem,
                        }),
                    );
                }
                details = Some(det);
                patched = true;
            }
        }

        if patched {
            Some(ToolResultPatch {
                content: Some(content),
                details,
                is_error,
                terminate,
            })
        } else {
            None
        }
    }));
    eprintln!("[post_edit_verify: on after edit/write/edit_block/ast_edit]");
}

fn should_debounce_post_edit(last_run_ms: &AtomicU64) -> bool {
    let prev = last_run_ms.load(Ordering::Relaxed);
    if prev == 0 {
        return false;
    }
    let now = unix_ms();
    now.saturating_sub(prev) < POST_EDIT_VERIFY_DEBOUNCE_SECS.saturating_mul(1000)
}

fn mark_post_edit_run(last_run_ms: &AtomicU64) {
    last_run_ms.store(unix_ms(), Ordering::Relaxed);
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn extract_edit_path(result: &pirs_ai::ToolResultMessage, cwd: &Path) -> Option<PathBuf> {
    let path = result
        .details
        .as_ref()
        .and_then(|d| d.get("path"))
        .and_then(|p| p.as_str())
        .map(PathBuf::from)
        .or_else(|| {
            result
                .content
                .iter()
                .filter_map(|b| b.as_text())
                .next()
                .and_then(|t| {
                    t.rsplit_once(" in ")
                        .map(|(_, p)| PathBuf::from(p.trim().lines().next().unwrap_or("").trim()))
                })
        })?;
    if path.as_os_str().is_empty() {
        return None;
    }
    Some(if path.is_absolute() {
        path
    } else {
        cwd.join(path)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn debounce_respects_window() {
        let last = AtomicU64::new(0);
        assert!(!should_debounce_post_edit(&last));
        mark_post_edit_run(&last);
        assert!(should_debounce_post_edit(&last));
        // Simulate older run.
        last.store(
            unix_ms().saturating_sub(POST_EDIT_VERIFY_DEBOUNCE_SECS * 1000 + 5_000),
            Ordering::Relaxed,
        );
        assert!(!should_debounce_post_edit(&last));
    }

    #[test]
    fn install_sets_after_hook_when_enabled() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        std::env::remove_var("PIRS_POST_EDIT_VERIFY");
        let mut hooks = Hooks::default();
        let dir = tempfile::tempdir().unwrap();
        install_post_edit_verify_hook(&mut hooks, dir.path());
        assert!(hooks.after_tool_call.is_some());
        std::env::set_var("PIRS_POST_EDIT_VERIFY", "0");
        let mut hooks2 = Hooks::default();
        install_post_edit_verify_hook(&mut hooks2, dir.path());
        assert!(hooks2.after_tool_call.is_none());
        std::env::remove_var("PIRS_POST_EDIT_VERIFY");
    }
}
