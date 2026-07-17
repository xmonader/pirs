use std::sync::Arc;

use anyhow::{bail, Context as _};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::{UnixListener, UnixStream};

use crate::storage;
use crate::supervisor::Supervisor;
use crate::types::{encode_message, IpcRequest, IpcResponse};

pub async fn serve(supervisor: Arc<Supervisor>) -> anyhow::Result<()> {
    let sock = storage::socket_path();
    if let Some(parent) = sock.parent() {
        std::fs::create_dir_all(parent)?;
    }
    if sock.exists() {
        match UnixStream::connect(&sock).await {
            Ok(_) => bail!("orchestrator already running at {}", sock.display()),
            Err(_) => {
                std::fs::remove_file(&sock)?;
            }
        }
    }
    let listener = UnixListener::bind(&sock)
        .with_context(|| format!("failed to bind {}", sock.display()))?;

    supervisor.recover_after_restart()?;
    let _ = storage::ensure_machine();

    let shutdown = shutdown_signal();
    tokio::pin!(shutdown);

    loop {
        tokio::select! {
            accepted = listener.accept() => {
                match accepted {
                    Ok((stream, _)) => {
                        let sup = Arc::clone(&supervisor);
                        tokio::spawn(async move {
                            if let Err(e) = handle_connection(stream, sup).await {
                                tracing::warn!("connection error: {e}");
                            }
                        });
                    }
                    Err(e) => tracing::warn!("accept error: {e}"),
                }
            }
            _ = &mut shutdown => {
                break;
            }
        }
    }

    supervisor.stop_all().await;
    let _ = std::fs::remove_file(&sock);
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = tokio::signal::ctrl_c() => {}
            _ = term.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

async fn handle_connection(stream: UnixStream, supervisor: Arc<Supervisor>) -> anyhow::Result<()> {
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);
    let mut first = String::new();
    let n = reader.read_line(&mut first).await?;
    if n == 0 {
        return Ok(());
    }
    let request: IpcRequest = match serde_json::from_str(first.trim()) {
        Ok(r) => r,
        Err(e) => {
            write_half
                .write_all(encode_message(&IpcResponse::error(format!("invalid request: {e}"))).as_bytes())
                .await?;
            return Ok(());
        }
    };

    match request {
        IpcRequest::RpcStream { instance_id } => {
            let (record, process, mut events) = match supervisor.open_stream(&instance_id) {
                Ok(v) => v,
                Err(e) => {
                    write_half
                        .write_all(encode_message(&IpcResponse::error(e.to_string())).as_bytes())
                        .await?;
                    return Ok(());
                }
            };
            write_half
                .write_all(
                    encode_message(&IpcResponse::RpcReady {
                        ok: true,
                        instance: record,
                    })
                    .as_bytes(),
                )
                .await?;
            bridge_stream(reader, write_half, process, &mut events).await
        }
        other => {
            let response = handle_request(other, &supervisor).await;
            write_half
                .write_all(encode_message(&response).as_bytes())
                .await?;
            Ok(())
        }
    }
}

async fn handle_request(request: IpcRequest, supervisor: &Arc<Supervisor>) -> IpcResponse {
    match request {
        IpcRequest::Spawn { cwd, label, env } => match supervisor.spawn(&cwd, label, env).await {
            Ok(instance) => IpcResponse::SpawnResult {
                ok: true,
                instance: Some(instance),
                error: None,
            },
            Err(e) => IpcResponse::SpawnResult {
                ok: false,
                instance: None,
                error: Some(e.to_string()),
            },
        },
        IpcRequest::List => IpcResponse::ListResult {
            ok: true,
            instances: supervisor.list(),
        },
        IpcRequest::Status { instance_id } => match supervisor.status(&instance_id) {
            Some(instance) => IpcResponse::StatusResult {
                ok: true,
                instance: Some(instance),
                error: None,
            },
            None => IpcResponse::error(format!("Unknown instance: {instance_id}")),
        },
        IpcRequest::Stop { instance_id } => match supervisor.stop(&instance_id).await {
            Ok(()) => IpcResponse::StopResult { ok: true, error: None },
            Err(e) => IpcResponse::StopResult {
                ok: false,
                error: Some(e.to_string()),
            },
        },
        IpcRequest::Rpc {
            instance_id,
            command,
        } => match supervisor.rpc(&instance_id, command).await {
            Ok(response) => IpcResponse::RpcResult {
                ok: true,
                response: Some(response),
                error: None,
            },
            Err(e) => IpcResponse::RpcResult {
                ok: false,
                response: None,
                error: Some(e.to_string()),
            },
        },
        IpcRequest::RpcStream { .. } => unreachable!("handled above"),
    }
}

async fn bridge_stream(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: tokio::net::unix::OwnedWriteHalf,
    process: Arc<crate::rpc_process::RpcProcess>,
    events: &mut tokio::sync::mpsc::UnboundedReceiver<serde_json::Value>,
) -> anyhow::Result<()> {
    let (line_tx, mut line_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let reader_task = tokio::spawn(async move {
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {
                    if line_tx.send(line.trim().to_string()).is_err() {
                        break;
                    }
                }
            }
        }
    });

    let process2 = Arc::clone(&process);
    let (resp_tx, mut resp_rx) = tokio::sync::mpsc::unbounded_channel::<serde_json::Value>();
    let command_task = tokio::spawn(async move {
        while let Some(line) = line_rx.recv().await {
            let Ok(v) = serde_json::from_str::<serde_json::Value>(&line) else {
                continue;
            };
            let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
            if ty == "extension_ui_response" {
                let _ = process2.write_raw(&v).await;
                continue;
            }
            match process2.request(v).await {
                Ok(resp) => {
                    let _ = resp_tx.send(resp);
                }
                Err(e) => {
                    let _ = resp_tx.send(serde_json::json!({
                        "type": "error",
                        "ok": false,
                        "error": e.to_string(),
                    }));
                    break;
                }
            }
        }
    });

    loop {
        tokio::select! {
            ev = events.recv() => {
                match ev {
                    Some(v) => {
                        if writer.write_all(encode_message(&v).as_bytes()).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                }
            }
            resp = resp_rx.recv() => {
                if let Some(v) = resp {
                    if writer.write_all(encode_message(&v).as_bytes()).await.is_err() {
                        break;
                    }
                }
            }
        }
    }

    reader_task.abort();
    command_task.abort();
    Ok(())
}
