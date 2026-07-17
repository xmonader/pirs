use anyhow::bail;
use clap::{Parser, Subcommand};
use pirs_orchestrator::client;
use pirs_orchestrator::supervisor::Supervisor;
use pirs_orchestrator::types::{encode_message, IpcRequest};

#[derive(Parser)]
#[command(name = "pirs-orchestrator", about = "Manage fleets of headless pirs instances")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the orchestrator daemon
    Serve,
    /// List instances
    List,
    /// Spawn a new instance
    Spawn {
        #[arg(long)]
        cwd: Option<String>,
        #[arg(long)]
        label: Option<String>,
    },
    /// Show one instance
    Status { instance_id: String },
    /// Stop an instance
    Stop { instance_id: String },
    /// Send a one-shot RPC command (JSON) to an instance
    Rpc { instance_id: String, json: String },
    /// Raw JSONL bridge to an instance: stdin -> socket, socket -> stdout
    RpcStream { instance_id: String },
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "warn".into()),
        )
        .init();

    let cli = Cli::parse();
    match cli.command {
        Command::Serve => {
            let supervisor = Supervisor::new();
            pirs_orchestrator::server::serve(supervisor).await
        }
        Command::List => one_shot(&IpcRequest::List).await,
        Command::Spawn { cwd, label } => {
            let cwd = match cwd {
                Some(c) => c,
                None => std::env::current_dir()?.to_string_lossy().to_string(),
            };
            one_shot(&IpcRequest::Spawn { cwd, label }).await
        }
        Command::Status { instance_id } => {
            one_shot(&IpcRequest::Status { instance_id }).await
        }
        Command::Stop { instance_id } => one_shot(&IpcRequest::Stop { instance_id }).await,
        Command::Rpc { instance_id, json } => {
            let command: serde_json::Value = serde_json::from_str(&json)?;
            one_shot(&IpcRequest::Rpc {
                instance_id,
                command,
            })
            .await
        }
        Command::RpcStream { instance_id } => rpc_stream(&instance_id).await,
    }
}

async fn one_shot(request: &IpcRequest) -> anyhow::Result<()> {
    let line = encode_message(request);
    let response = client::send_ipc_request(line.trim()).await?;
    let pretty: serde_json::Value = serde_json::from_str(&response)?;
    println!("{}", serde_json::to_string_pretty(&pretty)?);
    Ok(())
}

async fn rpc_stream(instance_id: &str) -> anyhow::Result<()> {
    let line = encode_message(&IpcRequest::RpcStream {
        instance_id: instance_id.to_string(),
    });
    let stream = client::connect_stream(line.trim()).await?;
    let (read_half, mut write_half) = stream.into_split();
    let mut reader = tokio::io::BufReader::new(read_half);

    use tokio::io::{AsyncBufReadExt, AsyncWriteExt};

    let mut ready_line = String::new();
    reader.read_line(&mut ready_line).await?;
    let ready: serde_json::Value = serde_json::from_str(ready_line.trim())?;
    if ready.get("ok").and_then(|v| v.as_bool()) != Some(true) {
        bail!("rpc_stream rejected: {ready}");
    }
    eprintln!("[connected to {instance_id}]");

    let mut stdin = tokio::io::BufReader::new(tokio::io::stdin());
    let mut stdout = tokio::io::stdout();

    let mut from_sock = String::new();
    let mut from_stdin = String::new();
    let mut stdin_open = true;
    loop {
        tokio::select! {
            n = reader.read_line(&mut from_sock) => {
                match n {
                    Ok(0) | Err(_) => break,
                    Ok(_) => {
                        stdout.write_all(from_sock.as_bytes()).await?;
                        stdout.flush().await?;
                        from_sock.clear();
                    }
                }
            }
            n = stdin.read_line(&mut from_stdin), if stdin_open => {
                match n {
                    Ok(0) | Err(_) => {
                        stdin_open = false;
                    }
                    Ok(_) => {
                        write_half.write_all(from_stdin.as_bytes()).await?;
                        from_stdin.clear();
                    }
                }
            }
        }
    }
    Ok(())
}
