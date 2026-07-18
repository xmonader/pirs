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
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
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

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct SessionMeta {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parent_entry: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

fn meta_path(session: &Path) -> PathBuf {
    session.with_extension("meta.json")
}

pub fn write_meta(session: &Path, meta: &SessionMeta) -> anyhow::Result<()> {
    std::fs::write(meta_path(session), serde_json::to_string_pretty(meta)?)?;
    Ok(())
}

pub fn read_meta(session: &Path) -> Option<SessionMeta> {
    let content = std::fs::read_to_string(meta_path(session)).ok()?;
    serde_json::from_str(&content).ok()
}

pub fn session_id(session: &Path) -> String {
    session
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_else(|| "unknown".to_string())
}

/// Fork: copy messages[0..=entry_idx] into a new session linked to the parent.
pub fn fork_session(
    current: &Path,
    entry_idx: Option<usize>,
) -> anyhow::Result<(PathBuf, Vec<Message>, SessionMeta)> {
    let content = std::fs::read_to_string(current)?;
    let mut messages = Vec::new();
    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }
        messages.push(serde_json::from_str::<Message>(line)?);
    }
    let idx = entry_idx
        .map(|i| i.min(messages.len().saturating_sub(1)))
        .unwrap_or_else(|| messages.len().saturating_sub(1));
    let kept: Vec<Message> = messages.into_iter().take(idx + 1).collect();

    let dir = current.parent().context("session has no parent dir")?;
    let new_id = format!(
        "{}_fork{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0),
        std::process::id()
    );
    let new_path = dir.join(format!("{new_id}.jsonl"));
    let meta = SessionMeta {
        parent_session: Some(session_id(current)),
        parent_entry: Some(idx),
        label: None,
    };
    write_meta(&new_path, &meta)?;
    append(&new_path, &kept)?;
    Ok((new_path, kept, meta))
}

pub fn lineage(current: &Path) -> Vec<(String, Option<String>, Option<usize>, usize)> {
    let mut out = Vec::new();
    let mut cursor = current.to_path_buf();
    for _ in 0..32 {
        let id = session_id(&cursor);
        let meta = read_meta(&cursor).unwrap_or_default();
        let entries = std::fs::read_to_string(&cursor)
            .map(|c| c.lines().filter(|l| !l.trim().is_empty()).count())
            .unwrap_or(0);
        let parent = meta.parent_session.clone();
        out.push((id, parent.clone(), meta.parent_entry, entries));
        let Some(parent) = parent else {
            break;
        };
        let found = cursor
            .parent()
            .map(|dir| {
                std::fs::read_dir(dir)
                    .map(|rd| {
                        rd.flatten()
                            .map(|e| e.path())
                            .filter(|p| {
                                p.file_stem()
                                    .map(|s| s.to_string_lossy() == parent)
                                    .unwrap_or(false)
                            })
                            .collect::<Vec<_>>()
                    })
                    .unwrap_or_default()
            })
            .unwrap_or_default();
        if found.is_empty() {
            break;
        }
        cursor = found[0].clone();
    }
    out
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
