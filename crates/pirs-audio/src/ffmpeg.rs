//! Optional ffmpeg helpers (format normalize). Soft-dep: missing ffmpeg is OK.

use std::path::{Path, PathBuf};
use std::process::Command;

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
    let status = Command::new("ffmpeg")
        .args([
            "-y",
            "-i",
            input.to_str().unwrap_or(""),
            "-ar",
            "16000",
            "-ac",
            "1",
        ])
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() && out.is_file() {
        Ok(out)
    } else {
        Ok(input.to_path_buf())
    }
}

/// Convert wav bytes to another container via ffmpeg (best-effort).
pub fn convert_audio(input: &Path, dst_ext: &str) -> anyhow::Result<PathBuf> {
    let Some(_) = which("ffmpeg") else {
        return Ok(input.to_path_buf());
    };
    let out = input.with_extension(dst_ext);
    let status = Command::new("ffmpeg")
        .args(["-y", "-i", input.to_str().unwrap_or("")])
        .arg(&out)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()?;
    if status.success() && out.is_file() {
        Ok(out)
    } else {
        Ok(input.to_path_buf())
    }
}
