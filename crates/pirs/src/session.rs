use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};
use pirs_ai::Message;

fn sessions_root() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(Path::new(&home).join(".pirs").join("sessions"))
}

fn encode_cwd(cwd: &Path) -> String {
    use std::hash::{Hash, Hasher};
    let s = cwd.to_string_lossy();
    // The readable part maps every non-alphanumeric char to '_', so distinct
    // paths (/a/b, /a.b, /a b) collide. Append a hash of the full path so their
    // sessions never share a directory (which would resume the wrong project).
    let mut readable: String = s
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if readable.len() > 80 {
        readable = readable[readable.len() - 80..].to_string();
    }
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    s.hash(&mut hasher);
    format!("{readable}-{:016x}", hasher.finish())
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

pub fn messages_from_file(path: &Path) -> anyhow::Result<Vec<Message>> {
    let content = std::fs::read_to_string(path)?;
    let lines: Vec<(usize, &str)> = content
        .lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty())
        .collect();
    let last = lines.len().saturating_sub(1);
    let mut messages = Vec::new();
    for (pos, (i, line)) in lines.iter().enumerate() {
        match serde_json::from_str::<Message>(line) {
            Ok(msg) => messages.push(msg),
            Err(e) => {
                if pos == last {
                    // append() isn't atomic: a crash mid-writeln! leaves a
                    // truncated final record. Tolerate it rather than making the
                    // whole session unresumable.
                    tracing::warn!(
                        "skipping truncated final line in {} (line {}): {e}",
                        path.display(),
                        i + 1
                    );
                } else {
                    return Err(anyhow::Error::new(e).context(format!(
                        "corrupt session {} at line {}",
                        path.display(),
                        i + 1
                    )));
                }
            }
        }
    }
    Ok(messages)
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
    let mut messages = messages_from_file(&latest)?;
    // Resume after compaction: if a summary marker exists, fold history to it
    // so resume continues from the compacted state, not the full pre-compaction transcript.
    if let Some(pos) = messages.iter().position(|m| {
        matches!(m, Message::User(u)
            if matches!(&u.content, pirs_ai::UserContent::Text(t)
                if t.starts_with("[Earlier conversation summarized by the agent]")))
    }) {
        messages.drain(..pos);
    }
    Ok((latest, messages))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_cwd_disambiguates_colliding_paths() {
        // These three collide under the naive underscore mapping.
        let a = encode_cwd(Path::new("/a/b"));
        let b = encode_cwd(Path::new("/a.b"));
        let c = encode_cwd(Path::new("/a b"));
        assert_ne!(a, b);
        assert_ne!(a, c);
        assert_ne!(b, c);
        // Stable across calls (same process/version) so resume finds the dir.
        assert_eq!(a, encode_cwd(Path::new("/a/b")));
    }

    #[test]
    fn truncated_final_line_is_tolerated() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let good = serde_json::to_string(&Message::user("hi")).unwrap();
        // Valid record, then a crash-truncated final line.
        std::fs::write(&path, format!("{good}\n{{\"role\":\"user\",\"cont")).unwrap();
        let msgs = messages_from_file(&path).unwrap();
        assert_eq!(msgs.len(), 1);
    }

    #[test]
    fn interior_corruption_still_errors() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("s.jsonl");
        let good = serde_json::to_string(&Message::user("hi")).unwrap();
        // Bad line in the interior (not the last) must hard-error.
        std::fs::write(&path, format!("{{garbage}}\n{good}\n")).unwrap();
        assert!(messages_from_file(&path).is_err());
    }
}
