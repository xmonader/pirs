use std::path::Path;
use std::process::Command;

use anyhow::{bail, Context};

pub struct BlameInfo {
    pub commit: String,
    pub summary: String,
    pub note: Option<String>,
}

/// Provenance for one line: which commit touched it, and (if a pirs session
/// annotated that commit via git notes) which session/turn/model wrote it.
pub fn blame_line(repo: &Path, file: &str, line: u32) -> anyhow::Result<BlameInfo> {
    let out = Command::new("git")
        .args([
            "blame",
            "-L",
            &format!("{line},{line}"),
            "--porcelain",
            "--",
            file,
        ])
        .current_dir(repo)
        .output()
        .context("git blame failed to run")?;
    if !out.status.success() {
        bail!("git blame: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let mut lines = text.lines();
    let header = lines.next().unwrap_or("");
    let commit = header.split_whitespace().next().unwrap_or("").to_string();
    if commit.is_empty() {
        bail!("no blame info for {file}:{line}");
    }
    let summary = lines
        .find(|l| l.starts_with("summary "))
        .map(|l| l.trim_start_matches("summary ").to_string())
        .unwrap_or_default();
    let note = Command::new("git")
        .args(["notes", "show", &commit])
        .current_dir(repo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty());
    Ok(BlameInfo {
        commit,
        summary,
        note,
    })
}

/// Pretty one-liner for the CLI.
pub fn format_blame(info: &BlameInfo) -> String {
    let short: String = info.commit.chars().take(10).collect();
    match &info.note {
        Some(note) if note.contains("pirs-session=") => {
            let get = |key: &str| {
                note.split_whitespace()
                    .find_map(|kv| kv.strip_prefix(key))
                    .unwrap_or("?")
                    .to_string()
            };
            format!(
                "{short} — model {} in session {} turn {} ({})",
                get("pirs-model="),
                get("pirs-session="),
                get("pirs-turn="),
                info.summary
            )
        }
        Some(note) => format!("{short} — {} [note: {}]", info.summary, note),
        None => format!("{short} — {} (no pirs provenance)", info.summary),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(repo: &Path, args: &[&str]) {
        let out = Command::new("git")
            .args(args)
            .current_dir(repo)
            .output()
            .unwrap();
        assert!(out.status.success(), "git {args:?}: {:?}", out.stderr);
    }

    #[test]
    fn blame_reads_note_provenance() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path();
        git(repo, &["init", "-q"]);
        git(repo, &["config", "user.email", "t@t"]);
        git(repo, &["config", "user.name", "t"]);
        std::fs::write(repo.join("f.txt"), "one\ntwo\nthree\n").unwrap();
        git(repo, &["add", "f.txt"]);
        git(repo, &["commit", "-qm", "add f"]);
        git(
            repo,
            &[
                "notes",
                "add",
                "-f",
                "-m",
                "pirs-session=s1 pirs-turn=3 pirs-model=qwen3",
                "HEAD",
            ],
        );

        let info = blame_line(repo, "f.txt", 2).unwrap();
        assert!(info.note.as_deref().unwrap().contains("pirs-turn=3"));
        let pretty = format_blame(&info);
        assert!(
            pretty.contains("model qwen3")
                && pretty.contains("session s1")
                && pretty.contains("turn 3"),
            "{pretty}"
        );

        // No note -> explicit absence, not an error.
        git(repo, &["notes", "remove", "HEAD"]);
        let info2 = blame_line(repo, "f.txt", 1).unwrap();
        assert!(info2.note.is_none());
        assert!(format_blame(&info2).contains("no pirs provenance"));
    }
}
