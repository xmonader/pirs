use std::collections::HashMap;
use std::path::Path;
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use anyhow::{bail, Context as _};
use serde_json::Value;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, oneshot};

type PendingMap = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, String>>>>>;

pub struct RpcProcess {
    stdin: tokio::sync::Mutex<tokio::process::ChildStdin>,
    pending: PendingMap,
    event_senders: Arc<Mutex<Vec<mpsc::UnboundedSender<Value>>>>,
    exit_watchers: Arc<Mutex<Vec<oneshot::Sender<i64>>>>,
    stderr: Arc<Mutex<String>>,
    next_id: AtomicU64,
    child_pid: Option<u32>,
    child: tokio::sync::Mutex<Child>,
}

impl RpcProcess {
    pub async fn spawn(cwd: &Path) -> anyhow::Result<Arc<Self>> {
        let bin = resolve_pirs_binary()?;
        let mut child = Command::new(&bin)
            .arg("--mode")
            .arg("rpc")
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("failed to spawn {} --mode rpc", bin.display()))?;

        let stdin = child.stdin.take().context("no stdin on child")?;
        let stdout = child.stdout.take().context("no stdout on child")?;
        let stderr = child.stderr.take().context("no stderr on child")?;

        let pending: PendingMap = Arc::new(Mutex::new(HashMap::new()));
        let event_senders: Arc<Mutex<Vec<mpsc::UnboundedSender<Value>>>> =
            Arc::new(Mutex::new(Vec::new()));
        let stderr_buf = Arc::new(Mutex::new(String::new()));
        let exit_watchers: Arc<Mutex<Vec<oneshot::Sender<i64>>>> =
            Arc::new(Mutex::new(Vec::new()));

        {
            let pending = Arc::clone(&pending);
            let event_senders = Arc::clone(&event_senders);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let Ok(v) = serde_json::from_str::<Value>(&line) else {
                        continue;
                    };
                    let is_response = v.get("type").and_then(|t| t.as_str()) == Some("response");
                    let id = v.get("id").and_then(|i| i.as_str()).map(|s| s.to_string());
                    if is_response {
                        if let Some(id) = id {
                            let tx = pending.lock().unwrap().remove(&id);
                            if let Some(tx) = tx {
                                let _ = tx.send(Ok(v));
                                continue;
                            }
                        }
                    }
                    let senders = event_senders.lock().unwrap();
                    for tx in senders.iter() {
                        let _ = tx.send(v.clone());
                    }
                }
            });
        }

        {
            let stderr_buf = Arc::clone(&stderr_buf);
            tokio::spawn(async move {
                let mut lines = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = lines.next_line().await {
                    let mut buf = stderr_buf.lock().unwrap();
                    if buf.len() < 64 * 1024 {
                        buf.push_str(&line);
                        buf.push('\n');
                    }
                }
            });
        }

        let child_pid = child.id();
        let process = Arc::new(RpcProcess {
            stdin: tokio::sync::Mutex::new(stdin),
            pending,
            event_senders,
            exit_watchers,
            stderr: stderr_buf,
            next_id: AtomicU64::new(0),
            child_pid,
            child: tokio::sync::Mutex::new(child),
        });

        {
            let process = Arc::clone(&process);
            tokio::spawn(async move {
                let status = process.child.lock().await.wait().await;
                let code = status.ok().and_then(|s| s.code()).unwrap_or(-1) as i64;
                let stderr = process.stderr.lock().unwrap().clone();
                let mut pending = process.pending.lock().unwrap();
                for (_, tx) in pending.drain() {
                    let _ = tx.send(Err(format!("instance exited (code {code}): {stderr}")));
                }
                let watchers = std::mem::take(&mut *process.exit_watchers.lock().unwrap());
                for tx in watchers {
                    let _ = tx.send(code);
                }
            });
        }

        Ok(process)
    }

    pub async fn request(&self, mut command: Value) -> anyhow::Result<Value> {
        if command.get("id").is_none() {
            let n = self.next_id.fetch_add(1, Ordering::SeqCst);
            command["id"] = Value::String(format!("pirs_orch_{n}_{}", uuid::Uuid::new_v4()));
        }
        let id = command
            .get("id")
            .and_then(|v| v.as_str())
            .context("command id must be a string")?
            .to_string();

        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        let line = format!("{}\n", serde_json::to_string(&command)?);
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        let response = rx
            .await
            .context("instance exited before responding")?
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(response)
    }

    pub async fn write_raw(&self, value: &Value) -> anyhow::Result<()> {
        let line = format!("{}\n", serde_json::to_string(value)?);
        self.stdin.lock().await.write_all(line.as_bytes()).await?;
        Ok(())
    }

    pub fn subscribe_events(&self) -> mpsc::UnboundedReceiver<Value> {
        let (tx, rx) = mpsc::unbounded_channel();
        self.event_senders.lock().unwrap().push(tx);
        rx
    }

    pub fn on_exit(&self) -> oneshot::Receiver<i64> {
        let (tx, rx) = oneshot::channel();
        self.exit_watchers.lock().unwrap().push(tx);
        rx
    }

    pub async fn dispose(&self) {
        if let Some(pid) = self.child_pid {
            #[cfg(unix)]
            unsafe {
                libc::kill(pid as i32, libc::SIGTERM);
            }
            let _ = pid;
        }
        let mut child = self.child.lock().await;
        let _ = child.wait().await;
    }
}

fn resolve_pirs_binary() -> anyhow::Result<std::path::PathBuf> {
    if let Ok(bin) = std::env::var("PIRS_RPC_BIN") {
        let path = std::path::PathBuf::from(bin);
        if path.exists() {
            return Ok(path);
        }
        bail!("PIRS_RPC_BIN does not exist: {}", path.display());
    }
    if let Ok(exe) = std::env::current_exe() {
        let sibling = exe.with_file_name(if cfg!(windows) { "pirs.exe" } else { "pirs" });
        if sibling.exists() {
            return Ok(sibling);
        }
    }
    Ok(std::path::PathBuf::from("pirs"))
}
