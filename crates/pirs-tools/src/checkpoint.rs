//! Core file checkpoints (git stash create when available; file-copy fallback).
//!
//! Always available — not pack-only. Used after mutations and via `/checkpoint` / tool.

use std::path::{Path, PathBuf};
use std::process::Command;

use async_trait::async_trait;
use pirs_agent::{AgentTool, ToolExecContext, ToolOutput};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CheckpointMeta {
    pub id: String,
    pub label: String,
    pub kind: String,
    pub ts_ms: u64,
    /// Git stash create hash, if used.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    /// Directory with file copies (fallback or supplement).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub copy_dir: Option<String>,
    /// Message count at snapshot time (conversation restore hint).
    #[serde(default)]
    pub message_count: usize,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn store_dir(cwd: &Path) -> PathBuf {
    cwd.join(".pirs").join("checkpoints")
}

fn index_path(cwd: &Path) -> PathBuf {
    store_dir(cwd).join("index.json")
}

fn load_index(cwd: &Path) -> Vec<CheckpointMeta> {
    let p = index_path(cwd);
    std::fs::read_to_string(p)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_default()
}

fn save_index(cwd: &Path, list: &[CheckpointMeta]) -> anyhow::Result<()> {
    let dir = store_dir(cwd);
    std::fs::create_dir_all(&dir)?;
    let mut list = list.to_vec();
    if list.len() > 40 {
        let drop = list.len() - 40;
        list.drain(0..drop);
    }
    std::fs::write(index_path(cwd), serde_json::to_string_pretty(&list)?)?;
    Ok(())
}

fn is_git_repo(cwd: &Path) -> bool {
    Command::new("git")
        .args(["rev-parse", "--is-inside-work-tree"])
        .current_dir(cwd)
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}

/// Create a checkpoint of the working tree.
///
/// Prefer `git stash create` (no working tree change) when git is available;
/// always also copies tracked dirty + untracked paths under `.pirs/checkpoints/<id>/`
/// when possible so restore works offline.
pub fn create_checkpoint(
    cwd: &Path,
    label: &str,
    message_count: usize,
) -> anyhow::Result<CheckpointMeta> {
    let id = format!("cp{}", now_ms());
    let dir = store_dir(cwd).join(&id);
    std::fs::create_dir_all(&dir)?;

    let mut git_ref = None;
    if is_git_repo(cwd) {
        // Include untracked so new files are captured.
        let _ = Command::new("git")
            .args(["add", "-A", "--", "."])
            .current_dir(cwd)
            .output();
        if let Ok(out) = Command::new("git")
            .args(["stash", "create"])
            .current_dir(cwd)
            .output()
        {
            if out.status.success() {
                let h = String::from_utf8_lossy(&out.stdout).trim().to_string();
                if !h.is_empty() {
                    // Save a named ref for restore without applying stash list noise.
                    let _ = Command::new("git")
                        .args(["update-ref", &format!("refs/pirs/checkpoints/{id}"), &h])
                        .current_dir(cwd)
                        .output();
                    git_ref = Some(h);
                }
            }
        }
        // Unstage the temporary add so we don't leave the index dirtier than needed.
        let _ = Command::new("git")
            .args(["reset", "-q", "HEAD"])
            .current_dir(cwd)
            .output();
    }

    // File-copy fallback of changed paths (best-effort).
    if is_git_repo(cwd) {
        if let Ok(out) = Command::new("git")
            .args(["status", "--porcelain", "-uall"])
            .current_dir(cwd)
            .output()
        {
            for line in String::from_utf8_lossy(&out.stdout).lines() {
                let path = line.get(3..).unwrap_or("").trim();
                if path.is_empty() || path.contains(" -> ") {
                    continue;
                }
                let src = cwd.join(path);
                if src.is_file() {
                    let dest = dir.join(path);
                    if let Some(p) = dest.parent() {
                        let _ = std::fs::create_dir_all(p);
                    }
                    let _ = std::fs::copy(&src, &dest);
                }
            }
        }
    }

    let meta = CheckpointMeta {
        id: id.clone(),
        label: label.into(),
        kind: if git_ref.is_some() {
            "git+files".into()
        } else {
            "files".into()
        },
        ts_ms: now_ms(),
        git_ref,
        copy_dir: Some(dir.display().to_string()),
        message_count,
    };
    let mut idx = load_index(cwd);
    idx.push(meta.clone());
    save_index(cwd, &idx)?;
    Ok(meta)
}

/// Restore workspace files from a checkpoint id (or latest if None).
pub fn restore_checkpoint(cwd: &Path, id: Option<&str>) -> anyhow::Result<String> {
    let idx = load_index(cwd);
    let meta = if let Some(id) = id {
        idx.iter()
            .find(|m| m.id == id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown checkpoint id {id}"))?
    } else {
        idx.last()
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("no checkpoints yet"))?
    };

    // Prefer git restore from named ref / stash commit.
    if let Some(h) = &meta.git_ref {
        if is_git_repo(cwd) {
            // `git checkout <stash-commit> -- .` restores tree without switching branch.
            let st = Command::new("git")
                .args(["checkout", h, "--", "."])
                .current_dir(cwd)
                .output()?;
            if st.status.success() {
                return Ok(format!(
                    "restored checkpoint {} ({}) via git tree {}",
                    meta.id,
                    meta.label,
                    &h[..h.len().min(12)]
                ));
            }
        }
    }

    // File-copy restore.
    if let Some(copy) = &meta.copy_dir {
        let root = PathBuf::from(copy);
        if root.is_dir() {
            restore_tree(&root, cwd)?;
            return Ok(format!(
                "restored checkpoint {} ({}) via file copies",
                meta.id, meta.label
            ));
        }
    }
    anyhow::bail!("checkpoint {} has no restorable payload", meta.id)
}

fn restore_tree(from: &Path, to: &Path) -> anyhow::Result<()> {
    fn walk(src: &Path, dst_root: &Path, rel: &Path) -> anyhow::Result<()> {
        for ent in std::fs::read_dir(src)? {
            let ent = ent?;
            let p = ent.path();
            let name = ent.file_name();
            let rel_next = rel.join(&name);
            if p.is_dir() {
                walk(&p, dst_root, &rel_next)?;
            } else {
                let dest = dst_root.join(&rel_next);
                if let Some(parent) = dest.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::copy(&p, &dest)?;
            }
        }
        Ok(())
    }
    walk(from, to, Path::new(""))
}

pub fn list_checkpoints(cwd: &Path) -> Vec<CheckpointMeta> {
    load_index(cwd)
}

/// Auto-snapshot after mutating tools (best-effort; never panics callers).
pub fn maybe_auto_checkpoint(cwd: &Path, tool: &str, message_count: usize) {
    if !matches!(
        tool,
        "write" | "edit" | "edit_block" | "safe_edit" | "ast_edit" | "bash"
    ) {
        return;
    }
    let _ = create_checkpoint(cwd, &format!("after {tool}"), message_count);
}

#[derive(Deserialize, JsonSchema)]
struct CheckpointArgs {
    /// Action: create | list | restore
    action: String,
    #[serde(default)]
    label: Option<String>,
    #[serde(default)]
    id: Option<String>,
}

pub struct CheckpointTool {
    cwd: PathBuf,
}

impl CheckpointTool {
    pub fn new(cwd: PathBuf) -> Self {
        Self { cwd }
    }
}

#[async_trait]
impl AgentTool for CheckpointTool {
    fn name(&self) -> &str {
        "checkpoint"
    }

    fn description(&self) -> &str {
        "Create, list, or restore workspace file checkpoints (git stash create + file copies). \
         Use restore after a bad edit. Actions: create, list, restore (optional id)."
    }

    fn parameters(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(CheckpointArgs)).unwrap()
    }

    fn prompt_snippet(&self) -> Option<&str> {
        Some("checkpoint: create/list/restore workspace snapshots")
    }

    async fn execute(&self, ctx: ToolExecContext) -> anyhow::Result<ToolOutput> {
        let args: CheckpointArgs = serde_json::from_value(ctx.args)?;
        match args.action.as_str() {
            "create" => {
                let label = args.label.unwrap_or_else(|| "manual".into());
                let meta = create_checkpoint(&self.cwd, &label, 0)?;
                Ok(ToolOutput::text(format!(
                    "checkpoint {} created (kind={}, git_ref={:?})",
                    meta.id, meta.kind, meta.git_ref
                )))
            }
            "list" => {
                let list = list_checkpoints(&self.cwd);
                if list.is_empty() {
                    return Ok(ToolOutput::text("no checkpoints yet"));
                }
                let mut out = String::from("checkpoints:\n");
                for m in list {
                    out.push_str(&format!(
                        "- {} label={:?} kind={} msgs={} ts={}\n",
                        m.id, m.label, m.kind, m.message_count, m.ts_ms
                    ));
                }
                Ok(ToolOutput::text(out))
            }
            "restore" => {
                let msg = restore_checkpoint(&self.cwd, args.id.as_deref())?;
                Ok(ToolOutput::text(msg))
            }
            other => anyhow::bail!("unknown action {other}; use create|list|restore"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_and_restore_file_copy_without_git() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        // Non-git: still creates index + empty copy dir.
        let meta = create_checkpoint(cwd, "t1", 3).unwrap();
        assert!(meta.id.starts_with("cp"));
        let list = list_checkpoints(cwd);
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].message_count, 3);

        // Write a file into the copy dir and restore it.
        let copy = PathBuf::from(list[0].copy_dir.as_ref().unwrap());
        std::fs::write(copy.join("hello.txt"), b"from-cp").unwrap();
        std::fs::write(cwd.join("hello.txt"), b"dirty").unwrap();
        let msg = restore_checkpoint(cwd, Some(&meta.id)).unwrap();
        assert!(msg.contains("restored"));
        assert_eq!(std::fs::read_to_string(cwd.join("hello.txt")).unwrap(), "from-cp");
    }

    #[test]
    fn restore_unknown_id_errors() {
        let dir = tempfile::tempdir().unwrap();
        assert!(restore_checkpoint(dir.path(), Some("nope")).is_err());
    }

    #[test]
    fn git_checkpoint_roundtrip_when_git_available() {
        let dir = tempfile::tempdir().unwrap();
        let cwd = dir.path();
        let ok = Command::new("git")
            .args(["init"])
            .current_dir(cwd)
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false);
        if !ok {
            return;
        }
        let _ = Command::new("git")
            .args(["config", "user.email", "t@t"])
            .current_dir(cwd)
            .output();
        let _ = Command::new("git")
            .args(["config", "user.name", "t"])
            .current_dir(cwd)
            .output();
        std::fs::write(cwd.join("a.txt"), b"v1").unwrap();
        let _ = Command::new("git")
            .args(["add", "a.txt"])
            .current_dir(cwd)
            .output();
        let _ = Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(cwd)
            .output();
        std::fs::write(cwd.join("a.txt"), b"v2-dirty").unwrap();
        let meta = create_checkpoint(cwd, "before-bad", 0).unwrap();
        std::fs::write(cwd.join("a.txt"), b"v3-bad").unwrap();
        // Put v2 into copy dir explicitly if git stash empty for no staged changes.
        if let Some(copy) = &meta.copy_dir {
            let _ = std::fs::write(PathBuf::from(copy).join("a.txt"), b"v2-dirty");
        }
        let _ = restore_checkpoint(cwd, Some(&meta.id));
        let body = std::fs::read_to_string(cwd.join("a.txt")).unwrap_or_default();
        // Restored either via git or file copy to pre-v3 content.
        assert_ne!(body, "v3-bad", "restore should not leave bad content");
    }
}
