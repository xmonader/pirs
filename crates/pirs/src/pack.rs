use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{bail, Context};

/// Derive a pack name from a git URL: basename minus .git.
pub fn pack_name_from_url(url: &str) -> String {
    let base = url
        .trim_end_matches('/')
        .rsplit('/')
        .next()
        .unwrap_or("pack");
    base.trim_end_matches(".git").to_string()
}

/// .rhai scripts in the repo root or one level down (extensions/, packs/).
pub fn collect_scripts(dir: &Path) -> Vec<PathBuf> {
    let mut out = Vec::new();
    let mut scan = |d: &Path| {
        if let Ok(rd) = std::fs::read_dir(d) {
            for e in rd.flatten() {
                let p = e.path();
                if p.extension().and_then(|x| x.to_str()) == Some("rhai") {
                    out.push(p);
                }
            }
        }
    };
    scan(dir);
    for sub in ["extensions", "packs"] {
        scan(&dir.join(sub));
    }
    out.sort();
    out
}

/// Copy scripts into the extensions dir. Refuses overwrites unless force.
/// Returns installed paths.
pub fn install_scripts(
    scripts: &[PathBuf],
    dest_dir: &Path,
    force: bool,
) -> anyhow::Result<Vec<PathBuf>> {
    std::fs::create_dir_all(dest_dir)?;
    let mut installed = Vec::new();
    for src in scripts {
        let name = src.file_name().unwrap();
        let dest = dest_dir.join(name);
        if dest.exists() && !force {
            bail!(
                "{} already exists (use --force to overwrite)",
                dest.display()
            );
        }
        std::fs::copy(src, &dest)
            .with_context(|| format!("copy {} -> {}", src.display(), dest.display()))?;
        installed.push(dest);
    }
    Ok(installed)
}

fn git(dir: Option<&Path>, args: &[&str]) -> anyhow::Result<std::process::Output> {
    let mut cmd = Command::new("git");
    cmd.args(args);
    if let Some(d) = dir {
        cmd.current_dir(d);
    }
    Ok(cmd.output()?)
}

/// Reject pack URLs that would let `git clone` execute arbitrary code.
///
/// Two vectors are closed here: git transport helpers (`ext::sh -c '...'`,
/// which run a shell at clone time) via the `::` check, and argument injection
/// (a URL parsed as a `git` option) via the leading-`-` check. The remaining
/// transport risk is defused at the git layer in `clone_pinned` via
/// `-c protocol.ext.allow=never` and the `--` argument terminator. A genuinely
/// unreachable/malformed URL is left for `git clone` itself to reject at the
/// network layer, so local-path and scp-style remotes keep working.
pub fn validate_pack_url(url: &str) -> anyhow::Result<()> {
    if url.is_empty() {
        bail!("empty pack url");
    }
    if url.starts_with('-') {
        bail!("pack url must not start with '-' (would be parsed as a git option): {url}");
    }
    if url.contains("::") {
        bail!("pack url uses a git transport helper ('::'), which is refused for safety: {url}");
    }
    Ok(())
}

/// A pin (branch/tag/sha) is passed as a positional arg to `git checkout`;
/// reject anything that could be parsed as an option or a transport helper.
fn validate_pin(pin: &str) -> anyhow::Result<()> {
    if pin.is_empty() {
        bail!("empty pin");
    }
    if pin.starts_with('-') {
        bail!("pin must not start with '-': {pin}");
    }
    if pin.contains("::") {
        bail!("pin must not contain '::': {pin}");
    }
    Ok(())
}

/// Clone url into a temp dir, optionally checking out and verifying a pin
/// (branch, tag, or full commit sha). Returns (tempdir, head_sha).
pub fn clone_pinned(url: &str, pin: Option<&str>) -> anyhow::Result<(tempfile::TempDir, String)> {
    validate_pack_url(url)?;
    if let Some(pin) = pin {
        validate_pin(pin)?;
    }
    let tmp = tempfile::tempdir()?;
    let repo = tmp.path().join("repo");
    let repo_str = repo.to_str().context("temp repo path is not valid UTF-8")?;
    // `-c protocol.*.allow` disables the transport helpers that turn a clone
    // into code execution; `--` stops a hostile url being read as an option.
    let mut clone_args: Vec<&str> = vec![
        "-c",
        "protocol.ext.allow=never",
        "-c",
        "protocol.file.allow=user",
        "clone",
        "-q",
    ];
    if pin.is_none() {
        // Full clone (no --depth) only when pinning an arbitrary sha.
        clone_args.extend_from_slice(&["--depth", "1"]);
    }
    clone_args.extend_from_slice(&["--", url, repo_str]);
    let out = git(None, &clone_args)?;
    if !out.status.success() {
        bail!("git clone: {}", String::from_utf8_lossy(&out.stderr).trim());
    }
    if let Some(pin) = pin {
        let out = git(Some(&repo), &["checkout", "-q", pin, "--"])?;
        if !out.status.success() {
            bail!(
                "git checkout {pin}: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            );
        }
    }
    let head = git(Some(&repo), &["rev-parse", "HEAD"])?.stdout;
    let head = String::from_utf8_lossy(&head).trim().to_string();
    // A 40-hex pin is a hard requirement: checked-out commit must equal it.
    if let Some(pin) = pin {
        if pin.len() == 40 && pin.chars().all(|c| c.is_ascii_hexdigit()) && head != pin {
            bail!("pin mismatch: checked out {head}, want {pin}");
        }
    }
    Ok((tmp, head))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_url_rejects_code_execution_vectors() {
        // git transport helper -> shell at clone time
        assert!(validate_pack_url("ext::sh -c 'id'").is_err());
        assert!(validate_pack_url("ext::sh /tmp/x").is_err());
        // argument injection
        assert!(validate_pack_url("--upload-pack=touch /tmp/x").is_err());
        assert!(validate_pack_url("-oProxyCommand=x").is_err());
        assert!(validate_pack_url("").is_err());
        // legitimate remotes and local paths pass; git rejects unreachable ones
        assert!(validate_pack_url("https://github.com/a/b.git").is_ok());
        assert!(validate_pack_url("git://example.com/a/b").is_ok());
        assert!(validate_pack_url("ssh://git@example.com/a/b").is_ok());
        assert!(validate_pack_url("git@github.com:a/b.git").is_ok());
        assert!(validate_pack_url("/tmp/local/repo").is_ok());
    }

    #[test]
    fn pin_rejects_injection() {
        assert!(validate_pin("--foo").is_err());
        assert!(validate_pin("ext::sh").is_err());
        assert!(validate_pin("").is_err());
        assert!(validate_pin("main").is_ok());
        assert!(validate_pin("v1.2.3").is_ok());
        assert!(validate_pin("a1b2c3d4e5f6a7b8c9d0e1f2a3b4c5d6e7f8a9b0").is_ok());
    }

    #[test]
    fn name_from_url() {
        assert_eq!(
            pack_name_from_url("https://github.com/a/red-team.git"),
            "red-team"
        );
        assert_eq!(pack_name_from_url("https://github.com/a/b/"), "b");
        assert_eq!(pack_name_from_url("git@x:y/z"), "z");
    }

    fn fixture_repo() -> (tempfile::TempDir, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = tmp.path().join("src");
        std::fs::create_dir_all(&repo).unwrap();
        let g = |args: &[&str]| {
            let out = Command::new("git")
                .args(args)
                .current_dir(&repo)
                .output()
                .unwrap();
            assert!(out.status.success(), "git {args:?}");
        };
        g(&["init", "-q"]);
        g(&["config", "user.email", "t@t"]);
        g(&["config", "user.name", "t"]);
        std::fs::write(
            repo.join("alpha.rhai"),
            "// caps: {\"exec\": [\"git\"]}\nfn on_event(t,d) { () }\n",
        )
        .unwrap();
        std::fs::create_dir_all(repo.join("extensions")).unwrap();
        std::fs::write(
            repo.join("extensions").join("beta.rhai"),
            "fn on_event(t,d) { () }\n",
        )
        .unwrap();
        std::fs::write(repo.join("README.md"), "not a script").unwrap();
        g(&["add", "-A"]);
        g(&["commit", "-qm", "c1"]);
        let head = String::from_utf8_lossy(
            &Command::new("git")
                .args(["rev-parse", "HEAD"])
                .current_dir(&repo)
                .output()
                .unwrap()
                .stdout,
        )
        .trim()
        .to_string();
        (tmp, head)
    }

    #[test]
    fn clone_collect_install_pinned() {
        let (src_tmp, head) = fixture_repo();
        let url = src_tmp.path().join("src").to_string_lossy().to_string();

        let (clone_tmp, got_head) = clone_pinned(&url, Some(&head)).unwrap();
        assert_eq!(got_head, head);

        let scripts = collect_scripts(&clone_tmp.path().join("repo"));
        assert_eq!(scripts.len(), 2, "{scripts:?}");

        let dest = tempfile::tempdir().unwrap();
        let installed = install_scripts(&scripts, dest.path(), false).unwrap();
        assert_eq!(installed.len(), 2);
        assert!(dest.path().join("alpha.rhai").exists());
        assert!(dest.path().join("beta.rhai").exists());

        // Overwrite refused without force.
        assert!(install_scripts(&scripts, dest.path(), false).is_err());
        assert!(install_scripts(&scripts, dest.path(), true).is_ok());
    }

    #[test]
    fn wrong_sha_pin_fails() {
        let (src_tmp, _head) = fixture_repo();
        let url = src_tmp.path().join("src").to_string_lossy().to_string();
        let bad = "0".repeat(40);
        assert!(clone_pinned(&url, Some(&bad)).is_err());
    }
}
