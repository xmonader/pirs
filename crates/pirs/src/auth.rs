use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Context as _;

fn auth_path() -> anyhow::Result<PathBuf> {
    let home = std::env::var("HOME").context("HOME not set")?;
    Ok(PathBuf::from(home).join(".pirs").join("auth.json"))
}

pub fn load() -> HashMap<String, String> {
    let Ok(path) = auth_path() else {
        return HashMap::new();
    };
    let Ok(content) = std::fs::read_to_string(path) else {
        return HashMap::new();
    };
    serde_json::from_str(&content).unwrap_or_default()
}

pub fn get(provider: &str) -> Option<String> {
    load().get(provider).cloned()
}

pub fn set(provider: &str, key: &str) -> anyhow::Result<()> {
    let path = auth_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut all = load();
    all.insert(provider.to_string(), key.to_string());
    std::fs::write(&path, serde_json::to_string_pretty(&all)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&path)?.permissions();
        perms.set_mode(0o600);
        std::fs::set_permissions(&path, perms)?;
    }
    Ok(())
}

/// flag > stored auth.json > environment variable
pub fn resolve(flag: Option<&str>, provider: &str, env_var: &str) -> Option<String> {
    flag.map(|s| s.to_string())
        .or_else(|| get(provider))
        .or_else(|| std::env::var(env_var).ok())
}

pub fn login(provider: &str) -> anyhow::Result<()> {
    let key = rpassword_from_tty(provider)?;
    if key.trim().is_empty() {
        anyhow::bail!("empty key");
    }
    set(provider, key.trim())?;
    println!("stored {provider} key in ~/.pirs/auth.json (mode 600)");
    Ok(())
}

fn rpassword_from_tty(provider: &str) -> anyhow::Result<String> {
    let mut rl = rustyline::DefaultEditor::new()?;
    Ok(rl.readline(&format!("{provider} API key: "))?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get_roundtrip() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        set("anthropic", "sk-test-123").unwrap();
        assert_eq!(get("anthropic").as_deref(), Some("sk-test-123"));
        let perms = std::fs::metadata(dir.path().join(".pirs/auth.json"))
            .unwrap()
            .permissions();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(perms.mode() & 0o777, 0o600);
        }
        let _ = perms;
    }

    #[test]
    fn resolve_order() {
        let _guard = crate::TEST_ENV_LOCK.lock().unwrap();
        let dir = tempfile::tempdir().unwrap();
        std::env::set_var("HOME", dir.path());
        std::env::remove_var("PIRS_TEST_KEY");
        set("openai", "stored").unwrap();
        assert_eq!(resolve(None, "openai", "PIRS_TEST_KEY").as_deref(), Some("stored"));
        std::env::set_var("PIRS_TEST_KEY", "env");
        assert_eq!(resolve(None, "openai", "PIRS_TEST_KEY").as_deref(), Some("stored"));
        assert_eq!(
            resolve(Some("flag"), "openai", "PIRS_TEST_KEY").as_deref(),
            Some("flag")
        );
    }
}
