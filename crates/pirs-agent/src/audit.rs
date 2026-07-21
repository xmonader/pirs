//! First-class action audit log (JSONL under `~/.pirs/audit.jsonl` by default).
//!
//! Always available — not pack-only. Disable with `PIRS_AUDIT=0`.

use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde_json::{json, Value};

use crate::events::{AgentEvent, Emit};

/// Where audit lines are written.
pub fn default_audit_path() -> PathBuf {
    if let Ok(p) = std::env::var("PIRS_AUDIT_PATH") {
        if !p.trim().is_empty() {
            return PathBuf::from(p);
        }
    }
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    PathBuf::from(home).join(".pirs").join("audit.jsonl")
}

pub fn audit_enabled() -> bool {
    !matches!(
        std::env::var("PIRS_AUDIT").as_deref(),
        Ok("0") | Ok("false") | Ok("off")
    )
}

/// Thread-safe JSONL writer for tool/agent events.
#[derive(Clone)]
pub struct AuditLog {
    path: PathBuf,
    lock: Arc<Mutex<()>>,
}

impl AuditLog {
    pub fn open(path: PathBuf) -> Self {
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        Self {
            path,
            lock: Arc::new(Mutex::new(())),
        }
    }

    pub fn default_open() -> Self {
        Self::open(default_audit_path())
    }

    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    pub fn append(&self, entry: Value) {
        if !audit_enabled() {
            return;
        }
        let _g = self.lock.lock().unwrap_or_else(|e| e.into_inner());
        let line = entry.to_string();
        if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(&self.path) {
            let _ = writeln!(f, "{line}");
        }
    }

    /// Record a tool call start.
    pub fn tool_start(&self, tool_call_id: &str, tool: &str, args: &Value) {
        self.append(json!({
            "ts": now_ms(),
            "kind": "tool_start",
            "tool_call_id": tool_call_id,
            "tool": tool,
            "args": args,
        }));
    }

    /// Record a tool result (truncated body for size).
    pub fn tool_end(
        &self,
        tool_call_id: &str,
        tool: &str,
        is_error: bool,
        text: &str,
        details: Option<&Value>,
    ) {
        let mut body = text.to_string();
        if body.chars().count() > 2000 {
            body = body.chars().take(2000).collect::<String>() + "…";
        }
        let mut entry = json!({
            "ts": now_ms(),
            "kind": "tool_end",
            "tool_call_id": tool_call_id,
            "tool": tool,
            "is_error": is_error,
            "text": body,
        });
        if let Some(d) = details {
            // Keep patch / path keys for audit of edits without full dump.
            if let Some(obj) = d.as_object() {
                let mut slim = serde_json::Map::new();
                for k in ["path", "patch", "firstChangedLine", "errorKind"] {
                    if let Some(v) = obj.get(k) {
                        let mut vv = v.clone();
                        if k == "patch" {
                            if let Some(s) = v.as_str() {
                                if s.chars().count() > 4000 {
                                    vv = Value::String(
                                        s.chars().take(4000).collect::<String>() + "…",
                                    );
                                }
                            }
                        }
                        slim.insert(k.to_string(), vv);
                    }
                }
                if !slim.is_empty() {
                    entry["details"] = Value::Object(slim);
                }
            }
        }
        self.append(entry);
    }

    pub fn agent_end(&self, n_messages: usize) {
        self.append(json!({
            "ts": now_ms(),
            "kind": "agent_end",
            "messages": n_messages,
        }));
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Listener that writes tool/agent events to the audit log (subscribe on Agent).
pub fn audit_listener(audit: AuditLog) -> Emit {
    Arc::new(move |ev: AgentEvent| {
        match &ev {
            AgentEvent::ToolExecutionStart {
                tool_call_id,
                tool_name,
                args,
            } => {
                audit.tool_start(tool_call_id, tool_name, args);
            }
            AgentEvent::ToolExecutionEnd {
                tool_call_id,
                tool_name,
                result,
            } => {
                audit.tool_end(
                    tool_call_id,
                    tool_name,
                    result.is_error,
                    &result.display_text(),
                    result.details.as_ref(),
                );
            }
            AgentEvent::AgentEnd { messages } => {
                audit.agent_end(messages.len());
            }
            _ => {}
        }
    })
}

/// Wrap an existing emit so audit lines are written for tool events.
pub fn wrap_emit(inner: Emit, audit: AuditLog) -> Emit {
    let audit_emit = audit_listener(audit);
    Arc::new(move |ev: AgentEvent| {
        audit_emit(ev.clone());
        inner(ev);
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let log = AuditLog::open(path.clone());
        log.tool_start("1", "bash", &json!({"command": "ls"}));
        log.tool_end("1", "bash", false, "ok", None);
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tool_start"));
        assert!(text.contains("tool_end"));
        assert!(text.contains("bash"));
    }
}
