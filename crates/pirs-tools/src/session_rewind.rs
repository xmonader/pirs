//! Conversation rewind: snapshot message counts and restore prior agent history.

use std::path::Path;
use std::sync::Mutex;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use pirs_ai::Message;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RewindSnapshot {
    pub id: String,
    pub label: String,
    pub message_count: usize,
    pub ts_ms: u64,
}

#[derive(Default)]
struct RewindState {
    /// Full history snapshots (messages JSON).
    snaps: Vec<(RewindSnapshot, Vec<Message>)>,
}

/// Process-global rewind store for the current agent run.
static STATE: std::sync::OnceLock<Mutex<RewindState>> = std::sync::OnceLock::new();

fn state() -> &'static Mutex<RewindState> {
    STATE.get_or_init(|| Mutex::new(RewindState::default()))
}

/// Capture a snapshot of current messages (call after each user turn).
pub fn snapshot(label: &str, messages: &[Message]) {
    let mut g = state().lock().unwrap();
    let id = format!("s{}", g.snaps.len() + 1);
    let meta = RewindSnapshot {
        id: id.clone(),
        label: label.into(),
        message_count: messages.len(),
        ts_ms: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
    };
    g.snaps.push((meta, messages.to_vec()));
    // Cap memory: keep last 30 snapshots.
    if g.snaps.len() > 30 {
        g.snaps.remove(0);
    }
}

/// Restore messages by snapshot id. Returns None if unknown.
pub fn restore(id: &str) -> Option<Vec<Message>> {
    let g = state().lock().unwrap();
    g.snaps
        .iter()
        .find(|(m, _)| m.id == id)
        .map(|(_, msgs)| msgs.clone())
}

/// Restore last snapshot (before current).
pub fn restore_last() -> Option<(RewindSnapshot, Vec<Message>)> {
    let g = state().lock().unwrap();
    if g.snaps.len() < 2 {
        return g.snaps.last().map(|(m, msgs)| (m.clone(), msgs.clone()));
    }
    // Second-to-last is "before this turn" when we snapshot after each user msg.
    g.snaps
        .get(g.snaps.len() - 2)
        .map(|(m, msgs)| (m.clone(), msgs.clone()))
}

pub fn list_snapshots() -> Vec<RewindSnapshot> {
    state()
        .lock()
        .unwrap()
        .snaps
        .iter()
        .map(|(m, _)| m.clone())
        .collect()
}

/// Persist snapshots index for session (optional durability).
pub fn persist_index(session_dir: &Path) -> anyhow::Result<()> {
    let path = session_dir.join("rewind_index.json");
    let list = list_snapshots();
    std::fs::create_dir_all(session_dir)?;
    std::fs::write(path, serde_json::to_string_pretty(&list)?)?;
    Ok(())
}

#[derive(Deserialize, JsonSchema)]
struct RewindArgs {
    /// Action: list | restore | undo
    action: String,
    /// Snapshot id for restore (e.g. s3)
    #[serde(default)]
    id: Option<String>,
}

/// Tool for agent-driven rewind (also used by /undo slash).
pub struct RewindTool;

#[async_trait]
impl AgentTool for RewindTool {
    fn name(&self) -> &str {
        "session_rewind"
    }

    fn description(&self) -> &str {
        "List or restore conversation rewind snapshots. Actions: list, restore (id), undo \
         (previous snapshot). File-level undo still uses git stash packs when loaded."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(RewindArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("session_rewind: list/restore/undo conversation snapshots")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: RewindArgs = serde_json::from_value(ctx.args)?;
        match args.action.as_str() {
            "list" => {
                let snaps = list_snapshots();
                if snaps.is_empty() {
                    return Ok(ToolOutput::text(
                        "no rewind snapshots yet (created each user turn)",
                    ));
                }
                let mut out = String::from("rewind snapshots:\n");
                for s in snaps {
                    out.push_str(&format!(
                        "- {} msgs={} label={:?} ts={}\n",
                        s.id, s.message_count, s.label, s.ts_ms
                    ));
                }
                out.push_str(
                    "Note: restore/undo apply only when the host reloads agent.messages \
                     (slash /undo or REPL integration).",
                );
                Ok(ToolOutput::text(out))
            }
            "undo" | "restore" => {
                // Tool alone cannot mutate Agent; return snapshot payload for host.
                let (meta, msgs) = if args.action == "undo" {
                    restore_last().ok_or_else(|| anyhow::anyhow!("no snapshot to undo"))?
                } else {
                    let id = args
                        .id
                        .ok_or_else(|| anyhow::anyhow!("restore requires id"))?;
                    let msgs = restore(&id).ok_or_else(|| anyhow::anyhow!("unknown id {id}"))?;
                    let meta = list_snapshots()
                        .into_iter()
                        .find(|s| s.id == id)
                        .ok_or_else(|| anyhow::anyhow!("unknown id"))?;
                    (meta, msgs)
                };
                Ok(ToolOutput::text(format!(
                    "REWIND_SNAPSHOT id={} messages={} label={:?}\n\
                     Host should replace agent.messages with this snapshot ({} messages).",
                    meta.id,
                    msgs.len(),
                    meta.label,
                    msgs.len()
                ))
                .with_details(serde_json::json!({
                    "rewind_id": meta.id,
                    "message_count": msgs.len(),
                })))
            }
            other => anyhow::bail!("unknown action {other}; use list|restore|undo"),
        }
    }
}

/// Host-side: apply undo to a message vec.
pub fn host_undo(messages: &mut Vec<Message>) -> anyhow::Result<String> {
    let (meta, msgs) =
        restore_last().ok_or_else(|| anyhow::anyhow!("no rewind snapshot available"))?;
    *messages = msgs;
    Ok(format!(
        "restored snapshot {} ({} messages, label={:?})",
        meta.id,
        messages.len(),
        meta.label
    ))
}
