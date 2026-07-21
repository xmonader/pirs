//! Multi-backend model registry for claw — **shared** with harness via `pirs_ai::registry_file`.
//!
//! Loads **user** `~/.pirs/config.toml` only (project backends stay on the `pirs`
//! harness with trust). Falls back to env OpenAI-compat when empty.

use std::sync::Arc;

use pirs_ai::{
    api_key_for_alias, build_routing_provider, first_available_backend_key, load_user_registry,
    OpenAiCompat, RegistryFile, LlmProvider,
};

pub use pirs_ai::{
    parse_from_config_value, registry_file_has_models, user_config_path, BackendEntry, ModelEntry,
    ServeEntry,
};

/// Re-export type name used by tests / callers.
pub type RegistryFileLocal = RegistryFile;

pub fn load_user_registry_file() -> RegistryFile {
    load_user_registry()
}

/// Resolve provider for a model name or registry alias.
pub fn resolve_llm(
    model: &str,
    max_retries: u32,
) -> anyhow::Result<(Arc<dyn LlmProvider>, Option<String>, bool)> {
    use crate::secrets::resolve_provider_and_key;

    // Model-aware env fallback: deepseek-* must not hit DashScope when both keys exist.
    let (base, env_key) = resolve_provider_and_key(Some(model));
    let default: Arc<dyn LlmProvider> =
        Arc::new(OpenAiCompat::new(base).with_max_retries(max_retries));

    let reg = load_user_registry();
    if !reg.models.is_empty() {
        if let Some(router) =
            build_routing_provider(&reg, Arc::clone(&default), env_key.clone(), max_retries)?
        {
            if router.has_alias(model) {
                let key = api_key_for_alias(&reg, model)
                    .or_else(|| first_available_backend_key(&reg))
                    .or(env_key);
                eprintln!(
                    "[pirs-claw registry: alias {model:?} via shared pirs_ai registry ({} model(s))]",
                    reg.models.len()
                );
                return Ok((router, key, true));
            }
        }
    }
    Ok((default, env_key, false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shared_parse_sample() {
        let sample = r#"
[[backends]]
name = "dashscope"
kind = "openai_compatible"
base_url = "https://example.com/v1"
api_key_env = "DASHSCOPE_API_KEY"

[[models]]
alias = "qwen3.5-plus"
serve = [{ backend = "dashscope", model = "qwen3.5-plus" }]
"#;
        let v: toml::Value = sample.parse().unwrap();
        let reg = parse_from_config_value(&v);
        assert_eq!(reg.backends.len(), 1);
        assert_eq!(reg.models[0].alias, "qwen3.5-plus");
    }
}
