//! Product-run observability: JSONL flight recorder + phase metadata.
//!
//! Reuses [`pirs_agent::trace::Recorder`] (same format as the bench harness) so
//! one-shot / REPL / strategy runs can leave a queryable trail of agent events
//! and strategy phase boundaries without a separate schema.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use pirs_agent::events::AgentEvent;
use pirs_agent::strategy::{PhaseReq, ToolScope};
use pirs_agent::trace::Recorder;
use pirs_agent::Agent;

/// Resolve the path for `--trace` / `--trace=PATH`.
/// - `None` → tracing off
/// - `Some("AUTO")` or `Some("")` → `~/.pirs/traces/<run_id>.jsonl`
/// - `Some(path)` → that path
pub fn resolve_trace_path(flag: Option<&str>, run_id: &str) -> Option<PathBuf> {
    match flag {
        None => None,
        Some("") | Some("AUTO") => {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
            let dir = Path::new(&home).join(".pirs").join("traces");
            let _ = std::fs::create_dir_all(&dir);
            Some(dir.join(format!("{run_id}.jsonl")))
        }
        Some(p) => {
            let path = PathBuf::from(p);
            if let Some(parent) = path.parent() {
                if !parent.as_os_str().is_empty() {
                    let _ = std::fs::create_dir_all(parent);
                }
            }
            Some(path)
        }
    }
}

pub fn make_run_id(session_stem: &str) -> String {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{session_stem}-{unix}-{}", std::process::id())
}

/// Open a recorder at `path` (or return None).
pub fn open_recorder(path: &Path, run_id: &str) -> anyhow::Result<Arc<Recorder>> {
    let unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let rec = Recorder::to_file(path, run_id, unix)
        .map_err(|e| anyhow::anyhow!("open trace {}: {e}", path.display()))?;
    eprintln!("[trace: {run_id} → {}]", path.display());
    Ok(rec)
}

/// Subscribe the agent so every [`AgentEvent`] is written under `phase`.
/// `phase` is a shared slot so strategy code can update it per phase.
pub fn attach_agent_trace(
    agent: &mut Agent,
    recorder: Arc<Recorder>,
    phase: Arc<std::sync::Mutex<String>>,
) {
    agent.subscribe(Arc::new(move |event: AgentEvent| {
        let phase = phase
            .lock()
            .map(|p| p.clone())
            .unwrap_or_else(|_| "main".into());
        // Skip high-volume streaming deltas to keep traces usable; still capture
        // starts/ends, tools, turns, compaction.
        match &event {
            AgentEvent::MessageUpdate { .. } | AgentEvent::ToolExecutionUpdate { .. } => {}
            _ => recorder.agent_event(&phase, &event),
        }
    }));
}

/// Record strategy phase start (model, scope, phase id).
pub fn record_phase_start(rec: &Recorder, req: &PhaseReq) {
    rec.event(
        "phase.start",
        serde_json::json!({
            "phase_id": req.phase_id,
            "model": req.model,
            "scope": match req.scope {
                ToolScope::ReadOnly => "readonly",
                ToolScope::Full => "full",
            },
            "fresh": req.fresh,
        }),
    );
}

/// Record strategy phase end with output length (not content — keep traces lean).
/// Called from the strategy one-shot path after each attempt (pairs with
/// [`record_phase_start`]).
pub fn record_phase_end(rec: &Recorder, phase_id: &str, output_chars: usize, ok: bool) {
    rec.event(
        "phase.end",
        serde_json::json!({
            "phase_id": phase_id,
            "output_chars": output_chars,
            "ok": ok,
        }),
    );
}

/// Snapshot of resolved model routing for the run header (aliases → backends).
pub fn record_run_config(
    rec: &Recorder,
    model: &str,
    plan_model: Option<&str>,
    strategy: Option<&str>,
    aliases: &[String],
) {
    rec.event(
        "run.config",
        serde_json::json!({
            "model": model,
            "plan_model": plan_model,
            "strategy": strategy,
            "model_aliases": aliases,
        }),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_trace_off_when_unset() {
        assert!(resolve_trace_path(None, "r1").is_none());
    }

    #[test]
    fn resolve_trace_auto_under_home() {
        let p = resolve_trace_path(Some("AUTO"), "sess-1").unwrap();
        assert!(p.ends_with("sess-1.jsonl"));
        assert!(p.to_string_lossy().contains(".pirs/traces"));
    }

    #[test]
    fn resolve_trace_explicit_path() {
        let p = resolve_trace_path(Some("/tmp/pirs-trace-test.jsonl"), "x").unwrap();
        assert_eq!(p, PathBuf::from("/tmp/pirs-trace-test.jsonl"));
    }

    #[test]
    fn phase_start_end_roundtrip_in_memory() {
        let (rec, buf) = Recorder::in_memory("obs-test");
        let req = PhaseReq {
            phase_id: "plan-exec#0".into(),
            system: "plan".into(),
            prompt: "p".into(),
            scope: ToolScope::ReadOnly,
            fresh: true,
            model: Some("strong".into()),
        };
        record_phase_start(&rec, &req);
        record_phase_end(&rec, "plan-exec#0", 12, true);
        let bytes = buf.lock().unwrap().clone();
        let text = String::from_utf8(bytes).unwrap();
        assert!(text.contains("phase.start"));
        assert!(text.contains("plan-exec#0"));
        assert!(text.contains("strong"));
        assert!(text.contains("phase.end"));
    }
}
