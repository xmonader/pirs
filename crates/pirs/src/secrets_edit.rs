//! Append/update entries in `~/.pirs/secrets.env` and optional backend snippets
//! in `~/.pirs/config.toml` (TUI/CLI setup helpers).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context as _};

pub fn secrets_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".pirs").join("secrets.env"))
}

pub fn user_config_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".pirs").join("config.toml"))
}

/// Set or replace `NAME=value` in secrets.env (mode 600). Also sets process env.
pub fn set_secret_env(name: &str, value: &str) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    let value = value.trim();
    if name.is_empty() || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        bail!("invalid env name {name:?} (use A-Z, 0-9, _)");
    }
    if value.is_empty() {
        bail!("empty value for {name}");
    }
    let path = secrets_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    let mut lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        let body = line.trim();
        let stripped = body.strip_prefix("export ").unwrap_or(body);
        if let Some((k, _)) = stripped.split_once('=') {
            if k.trim() == name {
                lines.push(format!("{name}={value}"));
                replaced = true;
                continue;
            }
        }
        lines.push(line.to_string());
    }
    if !replaced {
        if !lines.is_empty() && !lines.last().map(|l| l.is_empty()).unwrap_or(true) {
            // keep file tidy
        }
        lines.push(format!("{name}={value}"));
    }
    let mut out = lines.join("\n");
    if !out.ends_with('\n') {
        out.push('\n');
    }
    let tmp = path.with_extension("env.tmp");
    std::fs::write(&tmp, &out)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&tmp, perms)?;
    }
    std::fs::rename(&tmp, &path)?;
    std::env::set_var(name, value);
    Ok(path)
}

/// Append a `[[backends]]` block if `name` is not already present.
pub fn append_backend(
    name: &str,
    base_url: &str,
    api_key_env: &str,
    kind: &str,
) -> anyhow::Result<PathBuf> {
    let name = name.trim();
    let base_url = base_url.trim().trim_end_matches('/');
    let api_key_env = api_key_env.trim();
    let kind = if kind.trim().is_empty() {
        "openai_compatible"
    } else {
        kind.trim()
    };
    if name.is_empty() || base_url.is_empty() || api_key_env.is_empty() {
        bail!("usage: name, base_url, and api_key_env are required");
    }
    if name.contains(|c: char| !(c.is_ascii_alphanumeric() || c == '-' || c == '_')) {
        bail!("backend name must be alphanumeric / - / _");
    }
    let path = user_config_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(&path).unwrap_or_default();
    // Crude existence check
    if existing.contains(&format!("name = \"{name}\""))
        || existing.contains(&format!("name=\"{name}\""))
    {
        bail!("backend {name:?} already appears in {}", path.display());
    }
    let block = format!(
        r#"
[[backends]]
name = "{name}"
kind = "{kind}"
base_url = "{base_url}"
api_key_env = "{api_key_env}"
"#
    );
    let mut out = existing;
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str(&block);
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, &out)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

/// Well-known key envs for setup status.
pub fn setup_status_lines() -> Vec<String> {
    let mut lines = Vec::new();
    for env in pirs_ai::well_known_key_envs() {
        let set = std::env::var(env).ok().filter(|s| !s.is_empty()).is_some();
        lines.push(format!(
            "  {env:<22} {}",
            if set { "set" } else { "missing" }
        ));
    }
    lines
}

pub fn ensure_pirs_dir() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    let dir = PathBuf::from(home).join(".pirs");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_secret_roundtrip() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let path = set_secret_env("OPENROUTER_API_KEY", "sk-test").unwrap();
        assert!(path.exists());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.contains("OPENROUTER_API_KEY=sk-test"));
        set_secret_env("OPENROUTER_API_KEY", "sk-new").unwrap();
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(text.matches("OPENROUTER_API_KEY=").count(), 1);
        assert!(text.contains("sk-new"));
        assert_eq!(
            std::env::var("OPENROUTER_API_KEY").ok().as_deref(),
            Some("sk-new")
        );
    }

    #[test]
    fn append_backend_writes_block() {
        let _g = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        let path = append_backend(
            "openrouter-work",
            "https://openrouter.ai/api/v1",
            "OPENROUTER_WORK_API_KEY",
            "openai_compatible",
        )
        .unwrap();
        let text = std::fs::read_to_string(path).unwrap();
        assert!(text.contains("openrouter-work"));
        assert!(text.contains("OPENROUTER_WORK_API_KEY"));
    }
}
