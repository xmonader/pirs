use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use pirs_ai::Message;

fn sessions_root() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(Path::new(&home).join(".pirs").join("sessions"))
}

fn encode_cwd(cwd: &Path) -> String {
    let s = cwd.to_string_lossy();
    s.chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '-' { c } else { '_' })
        .collect()
}

pub fn session_path(cwd: &Path) -> anyhow::Result<PathBuf> {
    let dir = sessions_root()?.join(encode_cwd(cwd));
    std::fs::create_dir_all(&dir)?;
    let id = format!(
        "{}_{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );
    Ok(dir.join(format!("{id}.jsonl")))
}

pub fn append(path: &Path, messages: &[Message]) -> anyhow::Result<()> {
    use std::io::Write;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for msg in messages {
        let line = serde_json::to_string(msg)?;
        writeln!(file, "{line}")?;
    }
    Ok(())
}

pub fn load_latest(cwd: &Path) -> anyhow::Result<(PathBuf, Vec<Message>)> {
    let dir = sessions_root()?.join(encode_cwd(cwd));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .with_context(|| format!("no sessions in {}", dir.display()))?
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|e| e.to_str()) == Some("jsonl"))
        .collect();
    files.sort();
    let Some(latest) = files.pop() else {
        bail!("no session files found");
    };
    let content = std::fs::read_to_string(&latest)?;
    let mut messages = Vec::new();
    for (i, line) in content.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }
        let msg: Message = serde_json::from_str(line)
            .with_context(|| format!("corrupt session {} at line {}", latest.display(), i + 1))?;
        messages.push(msg);
    }
    Ok((latest, messages))
}
