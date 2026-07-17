use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use anyhow::bail;
use serde_json::Value;

use crate::rpc_process::RpcProcess;
use crate::storage;
use crate::types::{now_iso, InstanceRecord, InstanceStatus};

const SESSION_REFRESH_COMMANDS: &[&str] = &[
    "new_session",
    "switch_session",
    "fork",
    "clone",
    "set_session_name",
    "prompt",
];

struct LiveInstance {
    record: InstanceRecord,
    process: Arc<RpcProcess>,
}

pub struct Supervisor {
    live: Arc<Mutex<HashMap<String, LiveInstance>>>,
}

impl Supervisor {
    pub fn new() -> Arc<Self> {
        Arc::new(Supervisor {
            live: Arc::new(Mutex::new(HashMap::new())),
        })
    }

    pub fn recover_after_restart(&self) -> anyhow::Result<()> {
        let mut all = storage::load_instances();
        let mut changed = false;
        for rec in all.iter_mut() {
            if matches!(rec.status, InstanceStatus::Online | InstanceStatus::Starting) {
                rec.status = InstanceStatus::Stopped;
                rec.last_seen_at = Some(now_iso());
                changed = true;
            }
        }
        if changed {
            storage::save_instances(&all)?;
        }
        Ok(())
    }

    pub async fn spawn(
        &self,
        cwd: &str,
        label: Option<String>,
        env: Option<std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<InstanceRecord> {
        let mut record = InstanceRecord {
            id: uuid::Uuid::new_v4().to_string(),
            status: InstanceStatus::Starting,
            cwd: cwd.to_string(),
            created_at: now_iso(),
            last_seen_at: Some(now_iso()),
            label,
            ..Default::default()
        };
        storage::upsert_instance(record.clone())?;

        let result = self.start_process(&mut record, env.as_ref()).await;
        match result {
            Ok(()) => {
                record.status = InstanceStatus::Online;
                record.last_seen_at = Some(now_iso());
                if let Some(inst) = self.live.lock().unwrap().get_mut(&record.id) {
                    inst.record = record.clone();
                }
                storage::upsert_instance(record.clone())?;
                Ok(record)
            }
            Err(e) => {
                record.status = InstanceStatus::Error;
                storage::upsert_instance(record.clone())?;
                record.status = InstanceStatus::Stopped;
                storage::upsert_instance(record.clone())?;
                self.live.lock().unwrap().remove(&record.id);
                Err(e)
            }
        }
    }

    async fn start_process(
        &self,
        record: &mut InstanceRecord,
        env: Option<&std::collections::HashMap<String, String>>,
    ) -> anyhow::Result<()> {
        let process = RpcProcess::spawn(std::path::Path::new(&record.cwd), env).await?;
        let id = record.id.clone();
        let live = Arc::clone(&self.live);
        let exit = process.on_exit();
        tokio::spawn(async move {
            let _ = exit.await;
            let mut map = live.lock().unwrap();
            if let Some(inst) = map.get_mut(&id) {
                if !matches!(inst.record.status, InstanceStatus::Stopping | InstanceStatus::Stopped) {
                    inst.record.status = InstanceStatus::Error;
                    inst.record.last_seen_at = Some(now_iso());
                    let rec = inst.record.clone();
                    let _ = storage::upsert_instance(rec);
                }
            }
        });

        let state = process
            .request(serde_json::json!({"type": "get_state"}))
            .await?;
        record.session_id = state
            .pointer("/data/sessionId")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        record.session_file = state
            .pointer("/data/sessionFile")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        record.last_seen_at = Some(now_iso());

        self.live.lock().unwrap().insert(
            record.id.clone(),
            LiveInstance {
                record: record.clone(),
                process,
            },
        );
        Ok(())
    }

    pub fn list(&self) -> Vec<InstanceRecord> {
        storage::load_instances()
    }

    pub fn status(&self, id: &str) -> Option<InstanceRecord> {
        let live = self.live.lock().unwrap();
        if let Some(inst) = live.get(id) {
            return Some(inst.record.clone());
        }
        storage::load_instances().into_iter().find(|r| r.id == id)
    }

    pub async fn stop(&self, id: &str) -> anyhow::Result<()> {
        let process = {
            let mut live = self.live.lock().unwrap();
            match live.get_mut(id) {
                Some(inst) => {
                    inst.record.status = InstanceStatus::Stopping;
                    inst.record.last_seen_at = Some(now_iso());
                    let _ = storage::upsert_instance(inst.record.clone());
                    Arc::clone(&inst.process)
                }
                None => bail!("Unknown instance: {id}"),
            }
        };
        process.dispose().await;
        {
            let mut live = self.live.lock().unwrap();
            live.remove(id);
        }
        storage::remove_instance(id)?;
        Ok(())
    }

    pub async fn rpc(&self, id: &str, command: Value) -> anyhow::Result<Value> {
        let process = self.process_for(id)?;
        let response = process.request(command.clone()).await?;
        self.maybe_refresh_session(id, &command).await;
        self.touch(id);
        Ok(response)
    }

    pub fn open_stream(
        &self,
        id: &str,
    ) -> anyhow::Result<(
        InstanceRecord,
        Arc<RpcProcess>,
        tokio::sync::mpsc::UnboundedReceiver<Value>,
    )> {
        let process = self.process_for(id)?;
        let record = self
            .status(id)
            .ok_or_else(|| anyhow::anyhow!("Unknown instance: {id}"))?;
        let events = process.subscribe_events();
        Ok((record, process, events))
    }

    fn process_for(&self, id: &str) -> anyhow::Result<Arc<RpcProcess>> {
        let live = self.live.lock().unwrap();
        live.get(id)
            .map(|i| Arc::clone(&i.process))
            .ok_or_else(|| anyhow::anyhow!("Unknown instance: {id}"))
    }

    async fn maybe_refresh_session(&self, id: &str, command: &Value) {
        let ty = command.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !SESSION_REFRESH_COMMANDS.contains(&ty) {
            return;
        }
        let Ok(process) = self.process_for(id) else {
            return;
        };
        if let Ok(state) = process
            .request(serde_json::json!({"type": "get_state"}))
            .await
        {
            let mut live = self.live.lock().unwrap();
            if let Some(inst) = live.get_mut(id) {
                inst.record.session_id = state
                    .pointer("/data/sessionId")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                inst.record.session_file = state
                    .pointer("/data/sessionFile")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                inst.record.last_seen_at = Some(now_iso());
                let _ = storage::upsert_instance(inst.record.clone());
            }
        }
    }

    fn touch(&self, id: &str) {
        let mut live = self.live.lock().unwrap();
        if let Some(inst) = live.get_mut(id) {
            inst.record.last_seen_at = Some(now_iso());
        }
    }

    pub async fn stop_all(&self) {
        let ids: Vec<String> = self.live.lock().unwrap().keys().cloned().collect();
        for id in ids {
            let _ = self.stop(&id).await;
        }
    }
}
