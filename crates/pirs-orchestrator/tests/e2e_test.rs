use std::sync::Arc;

use pirs_orchestrator::client;
use pirs_orchestrator::supervisor::Supervisor;
use pirs_orchestrator::types::{encode_message, IpcRequest};
use serde_json::Value;

fn mock_child_path() -> String {
    format!(
        "{}/tests/mock_rpc_child.sh",
        env!("CARGO_MANIFEST_DIR")
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn orchestrator_end_to_end_over_uds() {
    recovery_flips_online_to_stopped().await;

    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("PIRS_ORCHESTRATOR_DIR", dir.path());
    std::env::set_var("PIRS_RPC_BIN", mock_child_path());

    let supervisor = Supervisor::new();
    let server = tokio::spawn(pirs_orchestrator::server::serve(Arc::clone(&supervisor)));
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;

    // spawn
    let cwd = std::env::current_dir().unwrap().to_string_lossy().to_string();
    let resp = client::send_ipc_request(
        encode_message(&IpcRequest::Spawn {
            cwd,
            label: Some("test".into()),
        })
        .trim(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["type"], "spawn_result");
    assert_eq!(v["ok"], true);
    let id = v["instance"]["id"].as_str().unwrap().to_string();
    assert_eq!(v["instance"]["status"], "online");
    assert_eq!(v["instance"]["sessionId"], "fake-sid-123");

    // list
    let resp = client::send_ipc_request(encode_message(&IpcRequest::List).trim())
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["instances"].as_array().unwrap().len(), 1);

    // status
    let resp = client::send_ipc_request(
        encode_message(&IpcRequest::Status {
            instance_id: id.clone(),
        })
        .trim(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["instance"]["status"], "online");

    // one-shot rpc
    let resp = client::send_ipc_request(
        encode_message(&IpcRequest::Rpc {
            instance_id: id.clone(),
            command: serde_json::json!({"type": "get_messages"}),
        })
        .trim(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["type"], "rpc_result");
    assert_eq!(v["ok"], true);
    assert_eq!(v["response"]["command"], "get_messages");
    assert_eq!(v["response"]["success"], true);

    // rpc to unknown instance errors
    let resp = client::send_ipc_request(
        encode_message(&IpcRequest::Rpc {
            instance_id: "nope".into(),
            command: serde_json::json!({"type": "get_state"}),
        })
        .trim(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["ok"], false);

    // stop
    let resp = client::send_ipc_request(
        encode_message(&IpcRequest::Stop {
            instance_id: id.clone(),
        })
        .trim(),
    )
    .await
    .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert_eq!(v["ok"], true);

    // record gone after stop
    let resp = client::send_ipc_request(encode_message(&IpcRequest::List).trim())
        .await
        .unwrap();
    let v: Value = serde_json::from_str(&resp).unwrap();
    assert!(v["instances"].as_array().unwrap().is_empty());

    server.abort();
}

async fn recovery_flips_online_to_stopped() {
    let dir = tempfile::tempdir().unwrap();
    std::env::set_var("PIRS_ORCHESTRATOR_DIR", dir.path());

    let rec = pirs_orchestrator::types::InstanceRecord {
        id: "dead".into(),
        status: pirs_orchestrator::types::InstanceStatus::Online,
        cwd: "/tmp".into(),
        created_at: "1".into(),
        ..Default::default()
    };
    pirs_orchestrator::storage::upsert_instance(rec).unwrap();

    let supervisor = Supervisor::new();
    supervisor.recover_after_restart().unwrap();

    let all = pirs_orchestrator::storage::load_instances();
    assert_eq!(
        all[0].status,
        pirs_orchestrator::types::InstanceStatus::Stopped
    );
}
