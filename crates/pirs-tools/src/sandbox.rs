use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;

#[derive(Debug, Clone, Default)]
pub struct ExecOutput {
    pub stdout: String,
    pub stderr: String,
    pub code: Option<i32>,
    pub timed_out: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SandboxError {
    #[error("exec failed: {0}")]
    Exec(String),
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("sandbox unavailable: {0}")]
    Unavailable(String),
}

#[async_trait]
pub trait Sandbox: Send + Sync {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ExecOutput, SandboxError>;

    fn name(&self) -> &'static str;
}

pub struct LocalSandbox;

#[async_trait]
impl Sandbox for LocalSandbox {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ExecOutput, SandboxError> {
        crate::bash::exec_local(command, cwd, timeout).await
    }

    fn name(&self) -> &'static str {
        "local"
    }
}

pub struct DockerSandbox {
    /// Existing container to exec into, or image to `docker run --rm` per call.
    pub container: Option<String>,
    pub image: String,
}

impl DockerSandbox {
    pub fn from_env() -> Option<Self> {
        let spec = std::env::var("PIRS_SANDBOX").ok()?;
        let spec = spec.strip_prefix("docker").unwrap_or(&spec);
        let (container, image) = match spec.strip_prefix(':') {
            Some(c) if !c.is_empty() => (Some(c.to_string()), String::new()),
            _ => (None, String::new()),
        };
        let image = if container.is_some() {
            image
        } else {
            std::env::var("PIRS_SANDBOX_IMAGE")
                .unwrap_or_else(|_| "debian:stable-slim".to_string())
        };
        Some(DockerSandbox { container, image })
    }

    async fn docker_available() -> bool {
        tokio::process::Command::new("docker")
            .arg("version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await
            .map(|s| s.success())
            .unwrap_or(false)
    }
}

#[async_trait]
impl Sandbox for DockerSandbox {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ExecOutput, SandboxError> {
        if !Self::docker_available().await {
            return Err(SandboxError::Unavailable(
                "docker not found or not running".into(),
            ));
        }
        let mut args: Vec<String> = Vec::new();
        if let Some(container) = &self.container {
            args.extend(["exec".into(), "-w".into(), cwd.to_string_lossy().to_string()]);
            args.push(container.clone());
        } else {
            args.extend([
                "run".into(),
                "--rm".into(),
                "-w".into(), "/work".into(),
                "-v".into(), format!("{}:/work", cwd.canonicalize()?.display()),
                "--network".into(), "none".into(),
            ]);
            args.push(self.image.clone());
        }
        args.extend(["/bin/bash".into(), "-c".into(), command.to_string()]);

        let mut cmd = tokio::process::Command::new("docker");
        cmd.args(&args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);

        let stdout_pipe = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(128);
        tokio::spawn(read_pipe(stdout_pipe, true, tx.clone()));
        tokio::spawn(read_pipe(stderr_pipe, false, tx));

        let wait = child.wait();
        let (status, timed_out) = match timeout {
            Some(t) => match tokio::time::timeout(t, wait).await {
                Ok(s) => (s?, false),
                Err(_) => {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                    let _ = child.wait().await;
                    (child.wait().await?, true)
                }
            },
            None => (wait.await?, false),
        };

        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some((is_out, chunk)) = rx.recv().await {
            if is_out {
                stdout.push_str(&chunk);
            } else {
                stderr.push_str(&chunk);
            }
        }

        Ok(ExecOutput {
            stdout,
            stderr,
            code: status.code(),
            timed_out,
        })
    }

    fn name(&self) -> &'static str {
        "docker"
    }
}

async fn read_pipe<R: tokio::io::AsyncRead + Unpin>(
    mut reader: R,
    is_out: bool,
    tx: tokio::sync::mpsc::Sender<(bool, String)>,
) {
    use tokio::io::AsyncReadExt;
    let mut buf = [0u8; 8192];
    loop {
        match reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                if tx
                    .send((is_out, String::from_utf8_lossy(&buf[..n]).into_owned()))
                    .await
                    .is_err()
                {
                    break;
                }
            }
            Err(_) => break,
        }
    }
}

pub struct SshSandbox {
    pub target: String,
}

#[async_trait]
impl Sandbox for SshSandbox {
    async fn exec(
        &self,
        command: &str,
        cwd: &Path,
        timeout: Option<Duration>,
    ) -> Result<ExecOutput, SandboxError> {
        let script = format!("cd {} 2>/dev/null || cd ~; {}", cwd.display(), command);
        let mut cmd = tokio::process::Command::new("ssh");
        cmd.args([
            "-o", "BatchMode=yes",
            "-o", "ConnectTimeout=10",
            "-o", "StrictHostKeyChecking=accept-new",
            &self.target,
            "bash", "-s",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
        #[cfg(unix)]
        {
            cmd.process_group(0);
        }
        let mut child = cmd.spawn()?;
        let pid = child.id().unwrap_or(0);
        if let Some(mut stdin) = child.stdin.take() {
            use tokio::io::AsyncWriteExt;
            let _ = stdin.write_all(script.as_bytes()).await;
        }

        let stdout_pipe = child.stdout.take().expect("piped");
        let stderr_pipe = child.stderr.take().expect("piped");
        let (tx, mut rx) = tokio::sync::mpsc::channel::<(bool, String)>(128);
        tokio::spawn(read_pipe(stdout_pipe, true, tx.clone()));
        tokio::spawn(read_pipe(stderr_pipe, false, tx));

        let wait = child.wait();
        let (status, timed_out) = match timeout {
            Some(t) => match tokio::time::timeout(t, wait).await {
                Ok(s) => (s?, false),
                Err(_) => {
                    #[cfg(unix)]
                    unsafe {
                        libc::kill(-(pid as i32), libc::SIGKILL);
                    }
                    let _ = child.wait().await;
                    (child.wait().await?, true)
                }
            },
            None => (wait.await?, false),
        };

        let mut stdout = String::new();
        let mut stderr = String::new();
        while let Some((is_out, chunk)) = rx.recv().await {
            if is_out {
                stdout.push_str(&chunk);
            } else {
                stderr.push_str(&chunk);
            }
        }

        Ok(ExecOutput {
            stdout,
            stderr,
            code: status.code(),
            timed_out,
        })
    }

    fn name(&self) -> &'static str {
        "ssh"
    }
}

pub fn from_env() -> std::sync::Arc<dyn Sandbox> {
    if let Ok(spec) = std::env::var("PIRS_SANDBOX") {
        if let Some(target) = spec.strip_prefix("ssh:") {
            if !target.is_empty() {
                return std::sync::Arc::new(SshSandbox {
                    target: target.to_string(),
                });
            }
        }
    }
    if let Some(docker) = DockerSandbox::from_env() {
        std::sync::Arc::new(docker)
    } else {
        std::sync::Arc::new(LocalSandbox)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_sandbox_exec() {
        let out = LocalSandbox
            .exec("echo hello && echo err >&2", Path::new("/tmp"), None)
            .await
            .unwrap();
        assert!(out.stdout.contains("hello"));
        assert!(out.stderr.contains("err"));
        assert_eq!(out.code, Some(0));
    }

    #[tokio::test]
    async fn ssh_backend_when_available() {
        let Ok(target) = std::env::var("PIRS_TEST_SSH_TARGET") else {
            eprintln!("PIRS_TEST_SSH_TARGET not set, skipping live ssh test");
            return;
        };
        let sb = SshSandbox { target };
        let out = sb
            .exec("echo remote-host: $(hostname)", std::path::Path::new("/tmp"), Some(Duration::from_secs(15)))
            .await
            .unwrap();
        assert!(out.stdout.contains("remote-host:"), "{:?}", out);
        assert_eq!(out.code, Some(0));
    }

    #[tokio::test]
    async fn docker_spec_parsing() {
        std::env::remove_var("PIRS_SANDBOX");
        std::env::remove_var("PIRS_SANDBOX_IMAGE");
        assert!(DockerSandbox::from_env().is_none());
        std::env::set_var("PIRS_SANDBOX", "docker:mybox");
        let sb = DockerSandbox::from_env().unwrap();
        assert_eq!(sb.container.as_deref(), Some("mybox"));
        std::env::set_var("PIRS_SANDBOX", "docker");
        let sb2 = DockerSandbox::from_env().unwrap();
        assert!(sb2.container.is_none());
        assert_eq!(sb2.image, "debian:stable-slim");
        std::env::remove_var("PIRS_SANDBOX");
    }
}
