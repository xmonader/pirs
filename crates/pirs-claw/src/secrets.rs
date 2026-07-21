//! Load `~/.pirs/secrets.env` (does not override existing env vars).

use std::path::PathBuf;

pub fn load_secrets_env() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = PathBuf::from(home).join(".pirs").join("secrets.env");
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let body = line.strip_prefix("export ").unwrap_or(line);
        let Some((k, v)) = body.split_once('=') else {
            continue;
        };
        let k = k.trim();
        if std::env::var_os(k).is_some() {
            continue;
        }
        let mut v = v.trim().to_string();
        if (v.starts_with('\'') && v.ends_with('\'')) || (v.starts_with('"') && v.ends_with('"')) {
            v = v[1..v.len() - 1].to_string();
        }
        if v.starts_with("${") && v.ends_with('}') {
            let refn = &v[2..v.len() - 1];
            v = std::env::var(refn).unwrap_or_default();
            if v.is_empty() {
                continue;
            }
        }
        // SAFETY: process startup / before concurrent workers use these keys.
        std::env::set_var(k, v);
    }
}

/// Resolve OpenAI-compatible base URL + API key from env (post-secrets load).
///
/// `model` selects a provider when several keys are present (e.g. DeepSeek
/// models must not ride the DashScope endpoint).
pub fn resolve_provider_and_key(model: Option<&str>) -> (Option<String>, Option<String>) {
    pirs_ai::resolve_openai_compat(model)
}
