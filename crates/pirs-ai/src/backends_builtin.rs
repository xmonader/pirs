//! Built-in backend catalog + curated portable model index.
//!
//! Backends unlock when their `api_key_env` is set. Portable models list
//! ordered serve targets; missing keys are skipped at resolve time.

use std::collections::HashMap;

use crate::registry_file::{BackendEntry, ModelEntry, RegistryFile, ServeEntry};

/// Default OpenRouter attribution headers (harmless if overridden).
fn openrouter_headers() -> HashMap<String, String> {
    let mut h = HashMap::new();
    h.insert(
        "HTTP-Referer".into(),
        "https://github.com/xmonader/pirs".into(),
    );
    h.insert("X-Title".into(), "pirs".into());
    h
}

fn be(
    name: &str,
    kind: &str,
    base_url: &str,
    api_key_env: &str,
    headers: HashMap<String, String>,
) -> BackendEntry {
    BackendEntry {
        name: name.into(),
        kind: kind.into(),
        base_url: base_url.into(),
        api_key_env: Some(api_key_env.into()),
        headers,
    }
}

fn openai_compat(name: &str, base_url: &str, api_key_env: &str) -> BackendEntry {
    be(
        name,
        "openai_compatible",
        base_url,
        api_key_env,
        HashMap::new(),
    )
}

/// Built-in backends (subscriptions). Users add more of the same kind by
/// choosing a new `name` + `api_key_env` (e.g. `openrouter-work`).
pub fn builtin_backends() -> Vec<BackendEntry> {
    vec![
        {
            let mut b = openai_compat(
                "openrouter",
                "https://openrouter.ai/api/v1",
                "OPENROUTER_API_KEY",
            );
            b.headers = openrouter_headers();
            b
        },
        openai_compat(
            "dashscope",
            "https://coding-intl.dashscope.aliyuncs.com/v1",
            "DASHSCOPE_API_KEY",
        ),
        openai_compat(
            "deepseek",
            "https://api.deepseek.com/v1",
            "DEEPSEEK_API_KEY",
        ),
        openai_compat("openai", "https://api.openai.com/v1", "OPENAI_API_KEY"),
        openai_compat("groq", "https://api.groq.com/openai/v1", "GROQ_API_KEY"),
        be(
            "anthropic",
            "anthropic",
            "https://api.anthropic.com",
            "ANTHROPIC_API_KEY",
            HashMap::new(),
        ),
    ]
}

fn model(id: &str, tier: &str, serve: &[(&str, &str)]) -> ModelEntry {
    ModelEntry {
        alias: id.into(),
        persona: None,
        tier: Some(tier.into()),
        ctx: None,
        caps: vec![],
        serve: serve
            .iter()
            .map(|(b, m)| ServeEntry {
                backend: (*b).into(),
                model: (*m).into(),
            })
            .collect(),
    }
}

/// Curated portable model index (bare names → ordered backend/remote list).
pub fn builtin_portable_models() -> Vec<ModelEntry> {
    vec![
        model(
            "qwen-plus",
            "balanced",
            &[
                ("dashscope", "qwen3.5-plus"),
                ("openrouter", "qwen/qwen3.5-plus"),
            ],
        ),
        model(
            "qwen3.5-plus",
            "balanced",
            &[
                ("dashscope", "qwen3.5-plus"),
                ("openrouter", "qwen/qwen3.5-plus"),
            ],
        ),
        model(
            "deepseek-v4-flash",
            "fast",
            &[
                ("openrouter", "deepseek/deepseek-v4-flash"),
                ("dashscope", "deepseek-v4-flash"),
                ("deepseek", "deepseek-chat"),
            ],
        ),
        model(
            "deepseek-chat",
            "fast",
            &[
                ("deepseek", "deepseek-chat"),
                ("openrouter", "deepseek/deepseek-chat"),
            ],
        ),
        model(
            "gpt-4o-mini",
            "fast",
            &[
                ("openai", "gpt-4o-mini"),
                ("openrouter", "openai/gpt-4o-mini"),
            ],
        ),
        model(
            "gpt-4o",
            "strong",
            &[("openai", "gpt-4o"), ("openrouter", "openai/gpt-4o")],
        ),
        model(
            "claude-sonnet",
            "strong",
            &[
                ("anthropic", "claude-sonnet-4-5"),
                ("openrouter", "anthropic/claude-sonnet-4.5"),
            ],
        ),
        model(
            "claude-sonnet-4-5",
            "strong",
            &[
                ("anthropic", "claude-sonnet-4-5"),
                ("openrouter", "anthropic/claude-sonnet-4.5"),
            ],
        ),
    ]
}

/// Lowest-layer registry: builtins only.
pub fn builtin_registry() -> RegistryFile {
    RegistryFile {
        backends: builtin_backends(),
        models: builtin_portable_models(),
    }
}

/// Whether a backend entry currently has a non-empty API key in the environment.
pub fn backend_key_present(b: &BackendEntry) -> bool {
    match &b.api_key_env {
        None => true,
        Some(env) => std::env::var(env).ok().filter(|s| !s.is_empty()).is_some(),
    }
}

/// Backends with keys set (or no key required).
pub fn active_backends(reg: &RegistryFile) -> Vec<&BackendEntry> {
    reg.backends
        .iter()
        .filter(|b| backend_key_present(b))
        .collect()
}

/// Portable models that have at least one serve target with an active key.
pub fn active_portable_models(reg: &RegistryFile) -> Vec<&ModelEntry> {
    let active: std::collections::HashSet<&str> = active_backends(reg)
        .into_iter()
        .map(|b| b.name.as_str())
        .collect();
    reg.models
        .iter()
        .filter(|m| m.serve.iter().any(|s| active.contains(s.backend.as_str())))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtins_include_openrouter_and_dashscope() {
        let reg = builtin_registry();
        assert!(reg.backends.iter().any(|b| b.name == "openrouter"));
        assert!(reg.backends.iter().any(|b| b.name == "dashscope"));
        assert!(reg.models.iter().any(|m| m.alias == "qwen-plus"));
    }
}
