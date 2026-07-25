//! Optional ffmpeg helpers (format normalize). Soft-dep: missing ffmpeg is OK.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context as _};

/// Default kill deadline for STT/TTS/ffmpeg subprocesses (AC3).
pub const DEFAULT_SUBPROCESS_TIMEOUT: Duration = Duration::from_secs(120);

pub fn have_ffmpeg() -> bool {
    which("ffmpeg").is_some()
}

pub fn which(name: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path) {
        let p = dir.join(name);
        if p.is_file() {
            return Some(p);
        }
    }
    None
}

/// Run a command with a hard wall-clock timeout; kill the child on expiry.
///
/// Unbounded `Command::output()` can hang the audio server forever on a stuck
/// engine (review AC3). All speech/ffmpeg spawns should go through this.
pub fn run_with_timeout(mut command: Command, timeout: Duration) -> anyhow::Result<Output> {
    let timeout = timeout.min(Duration::from_secs(600)).max(Duration::from_secs(1));
    command.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = command.spawn().context("spawn subprocess")?;
    let mut stdout_pipe = child.stdout.take().context("stdout")?;
    let mut stderr_pipe = child.stderr.take().context("stderr")?;
    let out_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stdout_pipe.read_to_end(&mut buf);
        buf
    });
    let err_h = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait().context("try_wait")? {
            Some(s) => break s,
            None if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                let _ = out_h.join();
                let _ = err_h.join();
                bail!(
                    "subprocess timed out after {}s",
                    timeout.as_secs()
                );
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let stdout = out_h.join().unwrap_or_default();
    let stderr = err_h.join().unwrap_or_default();
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

/// Convert audio to 16 kHz mono wav when ffmpeg is available and needed.
pub fn ensure_wav(input: &Path) -> anyhow::Result<PathBuf> {
    let ext = input
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if matches!(ext.as_str(), "wav" | "flac") {
        return Ok(input.to_path_buf());
    }
    let Some(_) = which("ffmpeg") else {
        return Ok(input.to_path_buf());
    };
    let out = input.with_extension("converted.wav");
    let mut cmd = Command::new("ffmpeg");
    cmd.args([
        "-y",
        "-i",
        input.to_str().unwrap_or(""),
        "-ar",
        "16000",
        "-ac",
        "1",
    ])
    .arg(&out)
    .stdout(Stdio::null())
    .stderr(Stdio::null());
    match run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT) {
        Ok(o) if o.status.success() && out.is_file() => Ok(out),
        _ => Ok(input.to_path_buf()),
    }
}

/// Convert wav bytes to another container via ffmpeg (best-effort).
pub fn convert_audio(input: &Path, dst_ext: &str) -> anyhow::Result<PathBuf> {
    let Some(_) = which("ffmpeg") else {
        return Ok(input.to_path_buf());
    };
    // Reject path traversal in extension (review: response_format as ext).
    let safe_ext = dst_ext
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .take(8)
        .collect::<String>();
    let safe_ext = if safe_ext.is_empty() {
        "wav".to_string()
    } else {
        safe_ext
    };
    let out = input.with_extension(&safe_ext);
    let mut cmd = Command::new("ffmpeg");
    cmd.args(["-y", "-i", input.to_str().unwrap_or("")])
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    match run_with_timeout(cmd, DEFAULT_SUBPROCESS_TIMEOUT) {
        Ok(o) if o.status.success() && out.is_file() => Ok(out),
        _ => Ok(input.to_path_buf()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_with_timeout_kills_hanging_child() {
        let mut cmd = Command::new("sleep");
        cmd.arg("30");
        let start = Instant::now();
        let err = run_with_timeout(cmd, Duration::from_secs(1)).unwrap_err().to_string();
        assert!(err.contains("timed out"), "{err}");
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "must not wait full sleep"
        );
    }

    #[test]
    fn run_with_timeout_captures_success() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("echo hello-timeout");
        let out = run_with_timeout(cmd, Duration::from_secs(5)).unwrap();
        assert!(out.status.success());
        assert!(String::from_utf8_lossy(&out.stdout).contains("hello-timeout"));
    }
}
