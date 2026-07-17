use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum InstanceStatus {
    Starting,
    Online,
    Stopping,
    Stopped,
    Error,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct InstanceRecord {
    pub id: String,
    pub status: InstanceStatus,
    pub cwd: String,
    pub created_at: String,
    pub last_seen_at: Option<String>,
    pub label: Option<String>,
    pub session_id: Option<String>,
    pub session_file: Option<String>,
}

impl Default for InstanceRecord {
    fn default() -> Self {
        InstanceRecord {
            id: String::new(),
            status: InstanceStatus::Starting,
            cwd: String::new(),
            created_at: String::new(),
            last_seen_at: None,
            label: None,
            session_id: None,
            session_file: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct MachineRecord {
    pub id: String,
    pub created_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_seen_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcRequest {
    Spawn {
        cwd: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        label: Option<String>,
    },
    List,
    Status {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    Stop {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
    Rpc {
        #[serde(rename = "instanceId")]
        instance_id: String,
        command: serde_json::Value,
    },
    RpcStream {
        #[serde(rename = "instanceId")]
        instance_id: String,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum IpcResponse {
    SpawnResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    ListResult {
        ok: bool,
        instances: Vec<InstanceRecord>,
    },
    StatusResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        instance: Option<InstanceRecord>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    StopResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RpcResult {
        ok: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        response: Option<serde_json::Value>,
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    RpcReady {
        ok: bool,
        instance: InstanceRecord,
    },
    Error {
        ok: bool,
        error: String,
    },
}

impl IpcResponse {
    pub fn error(msg: impl Into<String>) -> Self {
        IpcResponse::Error {
            ok: false,
            error: msg.into(),
        }
    }
}

pub fn encode_message(v: &impl Serialize) -> String {
    format!("{}\n", serde_json::to_string(v).unwrap_or_default())
}

pub fn now_iso() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    format!("{secs}")
}
