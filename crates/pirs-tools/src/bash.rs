use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::bail;
use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::process::Command;

use crate::truncate::{self, MAX_LINES};

#[derive(Deserialize, JsonSchema)]
struct BashArgs {
    /// Command to run in bash
    command: String,
    /// Timeout in seconds; the process tree is killed on expiry
    timeout: Option<u64>,
}

pub struct BashTool {
    cwd: PathBuf,
}

impl BashTool {
    pub fn new(cwd: PathBuf) -> Self {
        BashTool { cwd }
    }
}

#[async_trait]
impl AgentTool for BashTool {
    fn name(&self) -> &str {
        "bash"
    }

    fn description(&self) -> &str {
        "Run a bash command in the working directory. stdout and stderr are combined; long output is tail-truncated and the full log is written to a temp file."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(BashArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("bash: run shell commands (timeout in seconds optional)")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: BashArgs = serde_json::from_value(ctx.args.clone())?;
        if !self.cwd.exists() {
            bail!("working directory {} does not exist", self.cwd.display());
        }
        run_command(&self.cwd, &args.command, args.timeout, &ctx).await
    }
}

async fn run_command(
    cwd: &std::path::Path,
    command: &str,
    timeout_secs: Option<u64>,
    ctx: &ToolExecContext,
) -> anyhow::Result<ToolOutput> {
    let shell = std::env::var("PIRS_SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let mut child = Command::new(&shell);
    child
        .arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    #[cfg(unix)]
    {
        child.process_group(0);
    }
    let mut child = child.spawn()?;
    let pid = child.id().unwrap_or(0);

    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");
    let (tx, mut rx) = tokio::sync::mpsc::channel::<String>(128);
    tokio::spawn(read_chunks(stdout, tx.clone()));
    tokio::spawn(read_chunks(stderr, tx));

    let mut out = String::new();
    let mut status: Option<std::process::ExitStatus> = None;
    let mut pipes_open = true;
    let deadline = timeout_secs.map(|t| Instant::now() + Duration::from_secs(t));
    let timeout_sleep = tokio::time::sleep_until(
        deadline.unwrap_or_else(|| Instant::now() + Duration::from_secs(86400 * 365)).into(),
    );
    tokio::pin!(timeout_sleep);
    let has_deadline = deadline.is_some();
    let mut last_update = Instant::now();

    while pipes_open || status.is_none() {
        tokio::select! {
            chunk = rx.recv(), if pipes_open => {
                match chunk {
                    Some(c) => {
                        out.push_str(&c);
                        if last_update.elapsed() > Duration::from_millis(100) {
                            last_update = Instant::now();
                            let tail = truncate::tail(&out, 20);
                            ctx.emit_update(tail.text.trim_end().to_string());
                        }
                    }
                    None => pipes_open = false,
                }
            }
            s = child.wait(), if status.is_none() => {
                status = Some(s?);
            }
            _ = &mut timeout_sleep, if has_deadline => {
                kill_tree(pid);
                let _ = child.wait().await;
                bail!("Command timed out after {} seconds\n{}", timeout_secs.unwrap(), tail_with_footer(&out, &ctx.tool_call_id));
            }
            _ = ctx.cancel.cancelled() => {
                kill_tree(pid);
                let _ = child.wait().await;
                bail!("Command aborted\n{}", tail_with_footer(&out, &ctx.tool_call_id));
            }
        }
    }

    let text = tail_with_footer(&out, &ctx.tool_call_id);
    let code = status.and_then(|s| s.code());
    match code {
        Some(0) => Ok(ToolOutput::text(if text.is_empty() {
            "(no output)".to_string()
        } else {
            text
        })),
        Some(n) => bail!("{text}\nCommand exited with code {n}"),
        None => bail!("{text}\nCommand terminated by signal"),
    }
}

fn tail_with_footer(out: &str, call_id: &str) -> String {
    let window = truncate::tail(out, MAX_LINES);
    let mut text = window.text.trim_end_matches('\n').to_string();
    if window.truncated {
        let spill = spill_to_temp(out, call_id);
        text.push_str(&format!(
            "\n\n[Showing lines {}-{} of {}. Full output: {}]",
            window.start_line,
            window.end_line,
            window.total_lines,
            spill.display()
        ));
    }
    text
}

fn spill_to_temp(out: &str, call_id: &str) -> PathBuf {
    let path = std::env::temp_dir().join(format!(
        "pirs-bash-{}.log",
        sanitize_id(call_id)
    ));
    if std::fs::write(&path, out).is_err() {
        return PathBuf::from("<failed to write log>");
    }
    path
}

fn sanitize_id(id: &str) -> String {
    id.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' || c == '_' { c } else { '_' })
        .collect()
}

async fn read_chunks<R: AsyncRead + Unpin>(mut reader: R, tx: tokio::sync::mpsc::Sender<String>) {
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx.send(String::from_utf8_lossy(&buf[..n]).into_owned()).await.is_err() {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

fn kill_tree(pid: u32) {
    #[cfg(unix)]
    unsafe {
        libc::kill(-(pid as i32), libc::SIGKILL);
    }
    #[cfg(not(unix))]
    let _ = pid;
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    fn ctx(args: Value) -> ToolExecContext {
        ToolExecContext {
            tool_call_id: "t1".into(),
            args,
            cancel: CancellationToken::new(),
            on_update: None,
        }
    }

    #[tokio::test]
    async fn captures_stdout_and_stderr() {
        let tool = BashTool::new(std::env::temp_dir());
        let out = tool
            .execute(ctx(serde_json::json!({"command": "echo out; echo err >&2"})))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("out"));
        assert!(text.contains("err"));
    }

    #[tokio::test]
    async fn nonzero_exit_errors_with_code() {
        let tool = BashTool::new(std::env::temp_dir());
        let err = tool
            .execute(ctx(serde_json::json!({"command": "echo partial; exit 3"})))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("partial"));
        assert!(msg.contains("exited with code 3"));
    }

    #[tokio::test]
    async fn timeout_kills_process() {
        let tool = BashTool::new(std::env::temp_dir());
        let start = Instant::now();
        let err = tool
            .execute(ctx(serde_json::json!({"command": "sleep 30", "timeout": 1})))
            .await
            .unwrap_err();
        assert!(err.to_string().contains("timed out after 1 seconds"));
        assert!(start.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn long_output_truncated_and_spilled() {
        let tool = BashTool::new(std::env::temp_dir());
        let out = tool
            .execute(ctx(serde_json::json!({"command": "seq 1 5000"})))
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("Full output:"));
        assert!(text.contains("5000"));
        assert!(text.lines().count() < 2100);
    }
}
