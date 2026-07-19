use std::path::PathBuf;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use pirs_agent::jobs::{self, JobStatus};

/// Clamp a model-supplied wait timeout (seconds) to a sane ceiling. Without
/// this, `Instant::now() + Duration::from_secs(u64::MAX)` overflows and panics
/// the agent (silently killing a background delegate thread).
fn clamp_wait_secs(secs: u64) -> u64 {
    secs.min(86_400)
}

pub const DAEMON_PATTERNS: &[&str] = &[
    "flask run",
    "flask --app",
    "uvicorn",
    "gunicorn",
    "npm start",
    "npm run dev",
    "yarn dev",
    "pnpm dev",
    "cargo run",
    "go run",
    "rails server",
    "rails s",
    "python -m http.server",
    "http.server",
    "php -s",
    "serve",
    "watch",
    "tail -f",
    "docker-compose up",
    "docker compose up",
    "redis-server",
    "postgres",
    "nginx",
    "caddy",
    "traefik",
    "mongod",
    "rabbitmq-server",
    "kafka-server-start",
    "elasticsearch",
    "jupyter",
    "streamlit",
    "vite",
    "next dev",
    "nuxt dev",
    "hugo server",
    "jekyll serve",
    "live-server",
    "nodemon",
    "pm2 start",
];

pub fn looks_like_daemon(command: &str) -> bool {
    let cmd = command.to_ascii_lowercase();
    DAEMON_PATTERNS.iter().any(|p| cmd.contains(p))
}

const READY_PATTERNS: &[&str] = &[
    "running on http",
    "listening on",
    "listening at",
    "serving",
    "started server",
    "server started",
    "uvicorn running",
    "watching for changes",
    "ready in",
    "application startup complete",
    "compiled successfully",
    "localhost:",
    "127.0.0.1:",
    "0.0.0.0:",
];

fn has_ready_signal(output: &str) -> bool {
    let out = output.to_ascii_lowercase();
    READY_PATTERNS.iter().any(|p| out.contains(p))
}

fn extract_url(output: &str) -> Option<String> {
    for token in output.split_whitespace() {
        if token.starts_with("http://") || token.starts_with("https://") {
            return Some(token.trim_end_matches([',', '.', ';', ')']).to_string());
        }
    }
    None
}

#[derive(Deserialize, JsonSchema)]
struct NoArgs {}

#[derive(Deserialize, JsonSchema)]
struct JobOutputArgs {
    /// Job id
    id: u64,
    /// Max lines from the end
    limit: Option<usize>,
}

#[derive(Deserialize, JsonSchema)]
struct JobKillArgs {
    /// Job id
    id: u64,
}

#[derive(Deserialize, JsonSchema)]
struct JobSteerArgs {
    /// Job id
    id: u64,
    /// Message to steer the background agent with
    message: String,
}

pub struct JobsTool;
pub struct JobOutputTool;
pub struct JobKillTool;
pub struct JobSteerTool;

#[async_trait]
impl AgentTool for JobsTool {
    fn name(&self) -> &str {
        "jobs"
    }
    fn description(&self) -> &str {
        "List background jobs (bash jobs and background sub-agents) with their status."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(NoArgs)).unwrap()
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("jobs: list background jobs")
    }
    async fn execute(&self, _ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let lines = jobs::registry().list();
        if lines.is_empty() {
            return Ok(ToolOutput::text("no background jobs"));
        }
        Ok(ToolOutput::text(lines.join("\n")))
    }
}

#[async_trait]
impl AgentTool for JobOutputTool {
    fn name(&self) -> &str {
        "job_output"
    }
    fn description(&self) -> &str {
        "Read the current output of a background job (bash output, or a background agent's progress)."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(JobOutputArgs)).unwrap()
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("job_output: read a background job's output")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: JobOutputArgs = serde_json::from_value(ctx.args)?;
        let Some(job) = jobs::registry().get(args.id) else {
            anyhow::bail!("no such job: {}", args.id);
        };
        let (status_line, output_path, progress) = {
            let j = job.lock().unwrap();
            (j.status_line(), j.output_path.clone(), j.progress.clone())
        };
        if let Some(progress) = progress {
            let text = progress.lock().unwrap().clone();
            return Ok(ToolOutput::text(format!("{status_line}\n\n{text}")));
        }
        let content =
            std::fs::read_to_string(&output_path).unwrap_or_else(|_| "(no output yet)".to_string());
        let limit = args.limit.unwrap_or(50);
        let lines: Vec<&str> = content.lines().collect();
        let start = lines.len().saturating_sub(limit);
        let tail = lines[start..].join("\n");
        Ok(ToolOutput::text(format!("{status_line}\n\n{tail}")))
    }
}

#[async_trait]
impl AgentTool for JobKillTool {
    fn name(&self) -> &str {
        "job_kill"
    }
    fn description(&self) -> &str {
        "Kill a background job by id."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(JobKillArgs)).unwrap()
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: JobKillArgs = serde_json::from_value(ctx.args)?;
        let Some(job) = jobs::registry().get(args.id) else {
            anyhow::bail!("no such job: {}", args.id);
        };
        let (pid, kind) = {
            let j = job.lock().unwrap();
            (j.pid, j.kind)
        };
        jobs::registry().request_stop(args.id);
        match kind {
            jobs::JobKind::Bash => {
                if let Some(pid) = pid {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                }
            }
            jobs::JobKind::Agent => {
                let cancel = {
                    let j = job.lock().unwrap();
                    j.cancel.clone()
                };
                if let Some(slot) = cancel {
                    slot.lock().unwrap().cancel();
                }
            }
        }
        jobs::registry().set_status(args.id, JobStatus::Killed);
        Ok(ToolOutput::text(format!("job {} killed", args.id)))
    }
}

#[async_trait]
impl AgentTool for JobSteerTool {
    fn name(&self) -> &str {
        "job_steer"
    }
    fn description(&self) -> &str {
        "Send a steering message to a running background sub-agent (only agent jobs are steerable)."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(JobSteerArgs)).unwrap()
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("job_steer: send a message to a running background agent")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: JobSteerArgs = serde_json::from_value(ctx.args)?;
        jobs::registry()
            .steer(args.id, &args.message)
            .map_err(anyhow::Error::msg)?;
        Ok(ToolOutput::text(format!("steered job {}", args.id)))
    }
}

pub fn bash_job_output_path(id: u64) -> PathBuf {
    std::env::temp_dir().join(format!("pirs-job-{id}.log"))
}

pub fn spawn_bash_job(
    cwd: &std::path::Path,
    command: &str,
    auto_restart: bool,
) -> anyhow::Result<(u64, PathBuf)> {
    let shell = std::env::var("PIRS_SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
    let (id, job) = jobs::registry().register(
        jobs::JobKind::Bash,
        command.chars().take(80).collect(),
        PathBuf::new(),
        None,
    );
    let path = bash_job_output_path(id);
    let path_for_thread = path.clone();
    let out_file = std::fs::File::create(&path)?;
    let err_file = out_file.try_clone()?;

    let mut cmd = std::process::Command::new(&shell);
    cmd.arg("-c")
        .arg(command)
        .current_dir(cwd)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::from(out_file))
        .stderr(std::process::Stdio::from(err_file));
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }
    let child = cmd.spawn()?;
    {
        let mut j = job.lock().unwrap();
        j.pid = Some(child.id());
        j.output_path = path.clone();
    }
    // Stop flag lets job_kill prevent respawns: the watcher checks it after
    // every exit and during the restart backoff.
    let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    jobs::registry().register_stop_flag(id, std::sync::Arc::clone(&stop));
    let path = path_for_thread;
    let return_path = path.clone();
    let command_owned = command.to_string();
    let cwd_owned = cwd.to_path_buf();
    std::thread::spawn(move || {
        let mut backoff = 1u64;
        let mut current = child;
        loop {
            let code = current.wait().ok().and_then(|s| s.code()).unwrap_or(-1);
            if stop.load(std::sync::atomic::Ordering::SeqCst) {
                jobs::registry().set_status(id, JobStatus::Killed);
                return;
            }
            if !auto_restart {
                jobs::registry().set_status(id, JobStatus::Exited(code));
                jobs::registry().notify(format!(
                    "background job #{id} exited (code {code}): {command_owned}"
                ));
                return;
            }
            jobs::registry().notify(format!(
                "job #{id} exited (code {code}); restarting in {backoff}s: {command_owned}"
            ));
            // Interruptible backoff: a kill during the wait still prevents the
            // respawn.
            for _ in 0..backoff * 10 {
                if stop.load(std::sync::atomic::Ordering::SeqCst) {
                    jobs::registry().set_status(id, JobStatus::Killed);
                    return;
                }
                std::thread::sleep(std::time::Duration::from_millis(100));
            }
            backoff = (backoff * 2).min(30);
            let shell = std::env::var("PIRS_SHELL").unwrap_or_else(|_| "/bin/bash".to_string());
            let out_file = std::fs::OpenOptions::new().append(true).open(&path);
            let Ok(out_file) = out_file else { return };
            let Ok(err_file) = out_file.try_clone() else {
                return;
            };
            let mut cmd = std::process::Command::new(shell);
            cmd.arg("-c")
                .arg(&command_owned)
                .current_dir(&cwd_owned)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::from(out_file))
                .stderr(std::process::Stdio::from(err_file));
            #[cfg(unix)]
            {
                use std::os::unix::process::CommandExt;
                cmd.process_group(0);
            }
            match cmd.spawn() {
                Ok(new_child) => {
                    current = new_child;
                    if let Some(job) = jobs::registry().get(id) {
                        job.lock().unwrap().pid = Some(current.id());
                    }
                }
                Err(_) => {
                    jobs::registry().set_status(id, JobStatus::Exited(-1));
                    return;
                }
            }
        }
    });
    Ok((id, return_path))
}

pub struct JobWaitTool;

#[derive(Deserialize, JsonSchema)]
struct JobWaitArgs {
    /// Job id
    id: u64,
    /// Max seconds to wait (default 300)
    timeout: Option<u64>,
}

#[derive(Deserialize, JsonSchema)]
struct WaitReadyArgs {
    /// Job id to watch
    id: u64,
    /// Max seconds to wait for readiness (default 30)
    timeout: Option<u64>,
    /// Port to probe via TCP connect (optional; use when the server doesn't print a readiness line)
    port: Option<u16>,
}

#[async_trait]
impl AgentTool for JobWaitTool {
    fn name(&self) -> &str {
        "job_wait"
    }
    fn description(&self) -> &str {
        "Block until a background job finishes (or timeout) and return its output. Use instead of polling job_output repeatedly."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(JobWaitArgs)).unwrap()
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("job_wait: block until a background job finishes")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: JobWaitArgs = serde_json::from_value(ctx.args)?;
        let timeout = std::time::Duration::from_secs(clamp_wait_secs(args.timeout.unwrap_or(300)));
        match jobs::registry().wait(args.id, timeout).await {
            Some(_status) => {
                let job = jobs::registry()
                    .get(args.id)
                    .ok_or_else(|| anyhow::anyhow!("no such job"))?;
                let (line, path, progress) = {
                    let j = job.lock().unwrap();
                    (j.status_line(), j.output_path.clone(), j.progress.clone())
                };
                let content = if let Some(p) = progress {
                    p.lock().unwrap().clone()
                } else {
                    std::fs::read_to_string(&path).unwrap_or_else(|_| "(no output)".into())
                };
                let tail: String = content
                    .lines()
                    .rev()
                    .take(30)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                Ok(ToolOutput::text(format!(
                    "{line}

{tail}"
                )))
            }
            None => Ok(ToolOutput::text(format!(
                "job {} still running after {}s",
                args.id,
                args.timeout.unwrap_or(300)
            ))),
        }
    }
}

pub struct WaitReadyTool;

#[async_trait]
impl AgentTool for WaitReadyTool {
    fn name(&self) -> &str {
        "wait_ready"
    }
    fn description(&self) -> &str {
        "Wait until a long-running server job signals readiness (output patterns or a URL appearing), then report the address. Use after starting servers to know they're actually up."
    }
    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(WaitReadyArgs)).unwrap()
    }
    fn prompt_snippet(&self) -> Option<&str> {
        Some("wait_ready: wait for a server job to signal it's up")
    }
    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: WaitReadyArgs = serde_json::from_value(ctx.args)?;
        let deadline = std::time::Instant::now()
            + std::time::Duration::from_secs(clamp_wait_secs(args.timeout.unwrap_or(30)));
        loop {
            if let Some(port) = args.port {
                if tokio::net::TcpStream::connect(("127.0.0.1", port))
                    .await
                    .is_ok()
                {
                    return Ok(ToolOutput::text(format!(
                        "server is accepting connections on port {port}"
                    )));
                }
            }
            let job = jobs::registry()
                .get(args.id)
                .ok_or_else(|| anyhow::anyhow!("no such job"))?;
            let (status_line, path, status) = {
                let j = job.lock().unwrap();
                (j.status_line(), j.output_path.clone(), j.status.clone())
            };
            let output = std::fs::read_to_string(&path).unwrap_or_default();
            if has_ready_signal(&output) {
                let url = extract_url(&output)
                    .unwrap_or_else(|| "(address not parsed; check output)".to_string());
                return Ok(ToolOutput::text(format!(
                    "server is up: {url}
{status_line}"
                )));
            }
            if !matches!(status, JobStatus::Running) {
                let tail: String = output
                    .lines()
                    .rev()
                    .take(10)
                    .collect::<Vec<_>>()
                    .into_iter()
                    .rev()
                    .collect::<Vec<_>>()
                    .join(
                        "
",
                    );
                return Ok(ToolOutput::text(format!(
                    "job exited before becoming ready:
{tail}"
                )));
            }
            if std::time::Instant::now() > deadline {
                return Ok(ToolOutput::text(format!(
                    "no readiness signal after {}s; job may still be starting. Check job_output.
{}",
                    args.timeout.unwrap_or(30),
                    output
                        .lines()
                        .rev()
                        .take(5)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect::<Vec<_>>()
                        .join(
                            "
"
                        )
                )));
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    }
}

pub fn tools() -> Vec<Box<dyn AgentTool>> {
    vec![
        Box::new(JobsTool),
        Box::new(JobOutputTool),
        Box::new(JobKillTool),
        Box::new(JobSteerTool),
        Box::new(JobWaitTool),
        Box::new(WaitReadyTool),
    ]
}

#[cfg(test)]
mod daemon_tests {
    use super::*;
    use tokio_util::sync::CancellationToken;

    #[test]
    fn daemon_classification() {
        assert!(looks_like_daemon("flask run"));
        assert!(looks_like_daemon("uvicorn app:app --reload"));
        assert!(looks_like_daemon("npm start"));
        assert!(looks_like_daemon("python -m http.server 8000"));
        assert!(!looks_like_daemon("ls -la"));
        assert!(!looks_like_daemon("cargo build"));
    }

    #[tokio::test]
    async fn bash_auto_backgrounds_daemon_command() {
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::bash::BashTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"command": "python3 -m http.server 18977"}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        assert!(text.contains("auto-backgrounded"), "{text}");
        assert!(text.contains("background job #"));

        let jobs = jobs::registry().list();
        let job_line = jobs.iter().find(|l| l.contains("http.server")).unwrap();
        let id: u64 = job_line
            .trim_start_matches('#')
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();

        let ready = WaitReadyTool
            .execute(ToolExecContext {
                tool_call_id: "t2".into(),
                args: serde_json::json!({"id": id, "timeout": 10, "port": 18977}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let ready_text = ready.content[0].as_text().unwrap().to_string();
        assert!(
            ready_text.contains("server is up") || ready_text.contains("accepting connections"),
            "{ready_text}"
        );

        let status = jobs::registry()
            .wait(id, std::time::Duration::from_millis(50))
            .await;
        assert!(status.is_none(), "server should still be running");
        let _ = jobs::registry().steer(id, "x");
        JobKillTool
            .execute(ToolExecContext {
                tool_call_id: "t3".into(),
                args: serde_json::json!({"id": id}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn job_wait_returns_on_completion() {
        let dir = tempfile::tempdir().unwrap();
        let tool = crate::bash::BashTool::new(dir.path().to_path_buf());
        let out = tool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"command": "sleep 0.3; echo finished", "background": true}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await
            .unwrap();
        let text = out.content[0].as_text().unwrap();
        let id: u64 = text
            .split('#')
            .nth(1)
            .unwrap()
            .split(' ')
            .next()
            .unwrap()
            .parse()
            .unwrap();
        let result = jobs::registry()
            .wait(id, std::time::Duration::from_secs(5))
            .await;
        assert!(matches!(result, Some(JobStatus::Exited(0))), "{result:?}");
    }

    /// Kill an auto-restart job: the watcher must NOT respawn it. Regression
    /// test — previously job_kill SIGKILLed the process but the restart loop
    /// never checked the stop flag and revived it.
    #[tokio::test]
    async fn killed_auto_restart_job_does_not_respawn() {
        let dir = tempfile::tempdir().unwrap();
        let marker = dir.path().join("runs.log");
        let cmd = format!("echo run >> {}", marker.display());
        let (id, _path) = spawn_bash_job(dir.path(), &cmd, true).unwrap();

        // Let it run and restart at least once (initial backoff is 1s).
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let before = std::fs::read_to_string(&marker).unwrap_or_default();
        assert!(
            before.matches("run").count() >= 2,
            "expected at least one restart, got: {before:?}"
        );

        let kill = JobKillTool
            .execute(ToolExecContext {
                tool_call_id: "t".into(),
                args: serde_json::json!({"id": id}),
                cancel: CancellationToken::new(),
                on_update: None,
            })
            .await;
        assert!(kill.is_ok(), "kill failed: {kill:?}");

        // Past the next backoff window: no further runs may appear.
        tokio::time::sleep(std::time::Duration::from_millis(2500)).await;
        let after = std::fs::read_to_string(&marker).unwrap_or_default();
        assert_eq!(
            before.matches("run").count(),
            after.matches("run").count(),
            "job respawned after kill: {after:?}"
        );
        let status = jobs::registry()
            .get(id)
            .map(|j| j.lock().unwrap().status.clone());
        assert!(matches!(status, Some(JobStatus::Killed)), "{status:?}");
    }
}
