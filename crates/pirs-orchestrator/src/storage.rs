use std::path::PathBuf;

use anyhow::Context as _;

use crate::types::{InstanceRecord, MachineRecord};

pub fn orchestrator_dir() -> PathBuf {
    if let Ok(dir) = std::env::var("PIRS_ORCHESTRATOR_DIR") {
        return PathBuf::from(dir);
    }
    let base = std::env::var("PIRS_CONFIG_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| {
            let home = std::env::var("HOME").unwrap_or_else(|_| ".".to_string());
            PathBuf::from(home).join(".pirs")
        });
    base.join("orchestrator")
}

pub fn socket_path() -> PathBuf {
    orchestrator_dir().join("orchestrator.sock")
}

fn instances_path() -> PathBuf {
    orchestrator_dir().join("instances.json")
}

fn machine_path() -> PathBuf {
    orchestrator_dir().join("machine.json")
}

pub fn load_instances() -> Vec<InstanceRecord> {
    let path = instances_path();
    let Ok(content) = std::fs::read_to_string(&path) else {
        return Vec::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn save_instances(instances: &[InstanceRecord]) -> anyhow::Result<()> {
    let dir = orchestrator_dir();
    std::fs::create_dir_all(&dir)?;
    let path = instances_path();
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, serde_json::to_string_pretty(instances)?)?;
    std::fs::rename(&tmp, &path)?;
    Ok(())
}

pub fn upsert_instance(record: InstanceRecord) -> anyhow::Result<()> {
    let mut all = load_instances();
    match all.iter_mut().find(|r| r.id == record.id) {
        Some(slot) => *slot = record,
        None => all.push(record),
    }
    save_instances(&all)
}

pub fn remove_instance(id: &str) -> anyhow::Result<()> {
    let mut all = load_instances();
    all.retain(|r| r.id != id);
    save_instances(&all)
}

pub fn load_machine() -> Option<MachineRecord> {
    let content = std::fs::read_to_string(machine_path()).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn save_machine(record: &MachineRecord) -> anyhow::Result<()> {
    let dir = orchestrator_dir();
    std::fs::create_dir_all(&dir)?;
    std::fs::write(machine_path(), serde_json::to_string_pretty(record)?)
        .context("failed to write machine.json")
}

pub fn ensure_machine() -> anyhow::Result<MachineRecord> {
    if let Some(mut m) = load_machine() {
        m.last_seen_at = Some(crate::types::now_iso());
        save_machine(&m)?;
        return Ok(m);
    }
    let record = MachineRecord {
        id: uuid::Uuid::new_v4().to_string(),
        created_at: crate::types::now_iso(),
        last_seen_at: Some(crate::types::now_iso()),
        label: None,
    };
    save_machine(&record)?;
    Ok(record)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_instances() {
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("PIRS_ORCHESTRATOR_DIR", dir.path());
        let rec = InstanceRecord {
            id: "i1".into(),
            status: crate::types::InstanceStatus::Online,
            cwd: "/tmp".into(),
            created_at: "1".into(),
            ..Default::default()
        };
        upsert_instance(rec.clone()).unwrap();
        let all = load_instances();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0].id, "i1");
        let mut updated = rec;
        updated.status = crate::types::InstanceStatus::Stopped;
        upsert_instance(updated).unwrap();
        assert_eq!(
            load_instances()[0].status,
            crate::types::InstanceStatus::Stopped
        );
        remove_instance("i1").unwrap();
        assert!(load_instances().is_empty());
    }
}
