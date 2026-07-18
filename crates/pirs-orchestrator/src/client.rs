use anyhow::Context as _;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::UnixStream;

use crate::storage;

pub async fn send_ipc_request(line: &str) -> anyhow::Result<String> {
    let sock = storage::socket_path();
    let stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "failed to connect to {} (is the daemon running?)",
            sock.display()
        )
    })?;
    let (read_half, mut write_half) = stream.into_split();
    write_half.write_all(format!("{line}\n").as_bytes()).await?;
    let mut reader = BufReader::new(read_half);
    let mut response = String::new();
    reader.read_line(&mut response).await?;
    if response.is_empty() {
        anyhow::bail!("daemon closed connection without responding");
    }
    Ok(response.trim().to_string())
}

pub async fn connect_stream(line: &str) -> anyhow::Result<UnixStream> {
    let sock = storage::socket_path();
    let stream = UnixStream::connect(&sock).await.with_context(|| {
        format!(
            "failed to connect to {} (is the daemon running?)",
            sock.display()
        )
    })?;
    let mut stream = stream;
    stream.write_all(format!("{line}\n").as_bytes()).await?;
    Ok(stream)
}
