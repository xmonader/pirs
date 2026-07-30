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
        let created = !self.path.exists();
        if let Ok(mut f) = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
        {
            // Owner-only when we create the file (table #18 partial: perms).
            if created {
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    let _ = std::fs::set_permissions(
                        &self.path,
                        std::fs::Permissions::from_mode(0o600),
                    );
                }
            }
            let _ = writeln!(f, "{line}");
        }
    }

    /// Record a tool call start (args redacted for secret-shaped keys).
    pub fn tool_start(&self, tool_call_id: &str, tool: &str, args: &Value) {
        self.append(json!({
            "ts": now_ms(),
            "kind": "tool_start",
            "tool_call_id": tool_call_id,
            "tool": tool,
            "args": redact_value(args),
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

/// Key name fragments that mark secret-shaped fields (case-insensitive).
const SECRET_KEY_FRAGMENTS: &[&str] = &[
    "password",
    "passwd",
    "secret",
    "token",
    "api_key",
    "apikey",
    "authorization",
    "auth_header",
    "private_key",
    "access_key",
    "client_secret",
    "bearer",
];

/// True if a JSON object key looks secret-shaped.
pub fn is_secret_key_name(key: &str) -> bool {
    let k = key.to_ascii_lowercase();
    SECRET_KEY_FRAGMENTS.iter().any(|f| k.contains(f))
}

/// True if a string looks like a bearer token / API key payload.
pub fn looks_like_secret_string(s: &str) -> bool {
    let t = s.trim();
    if t.len() < 12 {
        return false;
    }
    let lower = t.to_ascii_lowercase();
    if lower.starts_with("sk-")
        || lower.starts_with("sk_")
        || lower.starts_with("rk-")
        || lower.starts_with("bearer ")
        || lower.starts_with("basic ")
    {
        return true;
    }
    // Long hex / base64-ish blobs often used as tokens
    if t.len() >= 32
        && t.chars().all(|c| {
            c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '+' || c == '/' || c == '='
        })
        && t.chars().any(|c| c.is_ascii_digit())
        && t.chars().filter(|c| c.is_ascii_alphabetic()).count() >= 8
    {
        // Avoid redacting normal paths/URLs
        if t.contains('/') && (t.starts_with('/') || t.contains("://")) {
            return false;
        }
        if t.contains(' ') {
            return false;
        }
        return true;
    }
    false
}

/// Redact secret-shaped string values in JSON (recursive). Non-string secrets become `"***"`.
pub fn redact_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (k, val) in map {
                if is_secret_key_name(k) {
                    out.insert(k.clone(), Value::String("***".into()));
                } else {
                    out.insert(k.clone(), redact_value(val));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(redact_value).collect()),
        Value::String(s) if looks_like_secret_string(s) => Value::String("***".into()),
        other => other.clone(),
    }
}

/// Listener that writes tool/agent events to the audit log (subscribe on Agent).
pub fn audit_listener(audit: AuditLog) -> Emit {
    Arc::new(move |ev: AgentEvent| match &ev {
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
    fn audit_create_is_owner_only_on_unix() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let log = AuditLog::open(path.clone());
        log.append(json!({"kind": "test"}));
        assert!(path.is_file());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "audit file should be 0600, got {mode:o}");
        }
    }

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

    #[test]
    fn redact_value_masks_secret_keys() {
        let v = json!({
            "command": "echo hi",
            "api_key": "sk-live-abc",
            "nested": {"token": "xyz", "path": "/tmp/x"},
            "Authorization": "Bearer super-secret",
            "raw": "sk-proj-abcdefghijklmnopqrstuv"
        });
        let r = redact_value(&v);
        assert_eq!(r["command"], "echo hi");
        assert_eq!(r["api_key"], "***");
        assert_eq!(r["nested"]["token"], "***");
        assert_eq!(r["nested"]["path"], "/tmp/x");
        assert_eq!(r["Authorization"], "***");
        assert_eq!(r["raw"], "***");
        assert!(!r.to_string().contains("sk-live"));
        assert!(!r.to_string().contains("super-secret"));
        assert!(!r.to_string().contains("sk-proj"));
    }

    #[test]
    fn tool_start_redacts_secret_args_on_disk() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("a.jsonl");
        let log = AuditLog::open(path.clone());
        log.tool_start(
            "1",
            "http",
            &json!({"url": "https://api.example", "api_token": "sekrit-value-99"}),
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("tool_start"));
        assert!(text.contains("***"));
        assert!(
            !text.contains("sekrit-value-99"),
            "audit must not persist secret arg values: {text}"
        );
        assert!(text.contains("https://api.example"));
    }
}
