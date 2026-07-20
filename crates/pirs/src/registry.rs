//! Named backends + model aliases for multi-subscription routing.
//!
//! Loaded from `[[backends]]` / `[[models]]` in user (and optionally project)
//! `config.toml`. Backends carry base URLs and key env names — treated like
//! `base_url` for security: project layer may not set backends. Model aliases
//! are inert labels and may live in either layer (project can name aliases that
//! point at user-defined backends).
//!
//! ```toml
//! [[backends]]
//! name = "openrouter"
//! kind = "openai_compatible"
//! base_url = "https://openrouter.ai/api/v1"
//! api_key_env = "OPENROUTER_API_KEY"
//! headers = { HTTP-Referer = "https://example.com", X-Title = "pirs" }
//!
//! [[backends]]
//! name = "dashscope"
//! kind = "openai_compatible"
//! base_url = "https://coding-intl.dashscope.aliyuncs.com/v1"
//! api_key_env = "DASHSCOPE_API_KEY"
//!
//! [[models]]
//! alias = "deepseek-v4-flash"
//! tier = "fast"
//! ctx = 1000000
//! serve = [{ backend = "openrouter", model = "deepseek/deepseek-v4-flash" }]
//!
//! [[models]]
//! alias = "qwen-plus"
//! serve = [{ backend = "dashscope", model = "qwen3.5-plus" }]
//! ```
//!
//! Then: `pirs --model qwen-plus --plan-model deepseek-v4-flash --strategy plan-exec "…"`

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::{anyhow, bail};
use pirs_ai::{
    AnthropicClient, BackendKind, LlmProvider, ModelRoute, OpenAiCompat, RoutingProvider,
};
use serde::Deserialize;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct RegistryFile {
    #[serde(default)]
    pub backends: Vec<BackendEntry>,
    #[serde(default)]
    pub models: Vec<ModelEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct BackendEntry {
    pub name: String,
    /// `openai_compatible` (default) or `anthropic`.
    #[serde(default = "default_kind")]
    pub kind: String,
    pub base_url: String,
    pub api_key_env: Option<String>,
    #[serde(default)]
    pub headers: HashMap<String, String>,
}

fn default_kind() -> String {
    "openai_compatible".into()
}

#[derive(Debug, Clone, Deserialize)]
pub struct ModelEntry {
    pub alias: String,
    /// Optional metadata (reserved for future persona/tier UX).
    #[allow(dead_code)]
    pub persona: Option<String>,
    pub tier: Option<String>,
    pub ctx: Option<u64>,
    #[serde(default)]
    #[allow(dead_code)]
    pub caps: Vec<String>,
    /// Ordered serve targets; first entry is primary (failover not yet wired).
    pub serve: Vec<ServeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServeEntry {
    pub backend: String,
    pub model: String,
}

/// Merge two registry layers: `over` wins on name/alias conflicts.
pub fn merge(base: RegistryFile, over: RegistryFile) -> RegistryFile {
    let mut backends = base.backends;
    for b in over.backends {
        if let Some(i) = backends.iter().position(|x| x.name == b.name) {
            backends[i] = b;
        } else {
            backends.push(b);
        }
    }
    let mut models = base.models;
    for m in over.models {
        if let Some(i) = models.iter().position(|x| x.alias == m.alias) {
            models[i] = m;
        } else {
            models.push(m);
        }
    }
    RegistryFile { backends, models }
}

/// Parse a TOML document that is only backends/models (tests + tooling).
#[cfg(test)]
pub fn parse_registry_toml(text: &str) -> anyhow::Result<RegistryFile> {
    toml::from_str(text).context("parse backends/models registry")
}

/// Extract registry tables from a full config.toml that also has model/provider.
/// Unknown top-level keys are ignored when we deserialize into RegistryFile if
/// we use `#[serde(deny_unknown_fields)]` — we don't, so extra keys error on
/// default toml unless we use a wrapper.
///
/// Prefer [`parse_from_config_value`] with a full toml Value.
pub fn parse_from_config_value(value: &toml::Value) -> RegistryFile {
    let table = match value.as_table() {
        Some(t) => t,
        None => return RegistryFile::default(),
    };
    let mut partial = toml::map::Map::new();
    if let Some(b) = table.get("backends") {
        partial.insert("backends".into(), b.clone());
    }
    if let Some(m) = table.get("models") {
        partial.insert("models".into(), m.clone());
    }
    if partial.is_empty() {
        return RegistryFile::default();
    }
    match toml::Value::Table(partial).try_into::<RegistryFile>() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("[warning: backends/models in config.toml ignored: {e}]");
            RegistryFile::default()
        }
    }
}

pub fn load_registry_layers(cwd: &std::path::Path) -> RegistryFile {
    let mut reg = RegistryFile::default();
    // User layer first (backends with secrets live here).
    if let Some(path) = crate::config_file::user_config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = text.parse::<toml::Value>() {
                reg = merge(reg, parse_from_config_value(&v));
            }
        }
    }
    // Project layer: models only (backends stripped for security).
    if let Some(path) = crate::config_file::find_project_config(cwd) {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = text.parse::<toml::Value>() {
                let mut project = parse_from_config_value(&v);
                if !project.backends.is_empty() {
                    eprintln!(
                        "[note: project .pirs/config.toml defines backends, which are \
                         user-config-only and were ignored — define backends in ~/.pirs/config.toml]"
                    );
                    project.backends.clear();
                }
                reg = merge(reg, project);
            }
        }
    }
    reg
}

/// Build live providers for each backend + a [`RoutingProvider`].
pub fn build_routing_provider(
    registry: &RegistryFile,
    default: Arc<dyn LlmProvider>,
    default_api_key: Option<String>,
    max_retries: u32,
) -> anyhow::Result<Option<Arc<RoutingProvider>>> {
    if registry.backends.is_empty() && registry.models.is_empty() {
        return Ok(None);
    }
    if registry.models.is_empty() {
        // Backends without models still useful if someone only uses raw ids on
        // the default provider — nothing to route.
        return Ok(None);
    }

    let mut backend_handles: HashMap<
        String,
        (
            Arc<dyn LlmProvider>,
            Option<String>,
            Vec<(String, String)>,
        ),
    > = HashMap::new();

    for b in &registry.backends {
        let kind = BackendKind::parse(&b.kind)
            .ok_or_else(|| anyhow!("backend {:?}: unknown kind {:?}", b.name, b.kind))?;
        let api_key = b
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok())
            .filter(|s| !s.is_empty());
        if b.api_key_env.is_some() && api_key.is_none() {
            eprintln!(
                "[warning: backend {:?} api_key_env {:?} is unset]",
                b.name,
                b.api_key_env.as_deref().unwrap_or("")
            );
        }
        let headers: Vec<(String, String)> = b.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
        let provider: Arc<dyn LlmProvider> = match kind {
            BackendKind::OpenaiCompatible => Arc::new(
                OpenAiCompat::new(Some(b.base_url.clone()))
                    .with_max_retries(max_retries)
                    .with_provider_name(b.name.clone()),
            ),
            BackendKind::Anthropic => Arc::new(
                AnthropicClient::new(Some(b.base_url.clone())).with_max_retries(max_retries),
            ),
        };
        backend_handles.insert(b.name.clone(), (provider, api_key, headers));
    }

    let mut routes = Vec::new();
    for m in &registry.models {
        let primary = m
            .serve
            .first()
            .ok_or_else(|| anyhow!("model alias {:?} has empty serve list", m.alias))?;
        if !backend_handles.contains_key(&primary.backend)
            && primary.backend != "default"
        {
            bail!(
                "model alias {:?} serves backend {:?} which is not defined in [[backends]]",
                m.alias,
                primary.backend
            );
        }
        routes.push(ModelRoute {
            alias: m.alias.clone(),
            backend: primary.backend.clone(),
            remote_model: primary.model.clone(),
            tier: m.tier.clone(),
            ctx: m.ctx,
        });
    }

    // If a route points at "default", map it to the CLI default handle by
    // inserting a synthetic backend entry.
    if routes.iter().any(|r| r.backend == "default") {
        backend_handles.insert(
            "default".into(),
            (Arc::clone(&default), default_api_key.clone(), vec![]),
        );
    }

    // Every serve backend must exist.
    for r in &routes {
        if !backend_handles.contains_key(&r.backend) {
            bail!(
                "model alias {:?} needs backend {:?} — add it under [[backends]]",
                r.alias,
                r.backend
            );
        }
    }

    Ok(Some(Arc::new(RoutingProvider::new(
        default,
        default_api_key,
        vec![],
        backend_handles,
        routes,
    ))))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[[backends]]
name = "openrouter"
kind = "openai_compatible"
base_url = "https://openrouter.ai/api/v1"
api_key_env = "OPENROUTER_API_KEY"
headers = { HTTP-Referer = "https://example.com", X-Title = "pirs" }

[[backends]]
name = "dashscope"
kind = "openai_compatible"
base_url = "https://coding-intl.dashscope.aliyuncs.com/v1"
api_key_env = "DASHSCOPE_API_KEY"

[[models]]
alias = "deepseek-v4-flash"
tier = "fast"
ctx = 1000000
serve = [{ backend = "openrouter", model = "deepseek/deepseek-v4-flash" }]

[[models]]
alias = "qwen-plus"
serve = [{ backend = "dashscope", model = "qwen3.5-plus" }]
"#;

    #[test]
    fn parses_backends_and_model_aliases() {
        let reg = parse_registry_toml(SAMPLE).unwrap();
        assert_eq!(reg.backends.len(), 2);
        assert_eq!(reg.models.len(), 2);
        let or = reg.backends.iter().find(|b| b.name == "openrouter").unwrap();
        assert!(or.headers.contains_key("X-Title"));
        let m = reg
            .models
            .iter()
            .find(|m| m.alias == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(m.serve[0].model, "deepseek/deepseek-v4-flash");
        assert_eq!(m.ctx, Some(1_000_000));
    }

    #[test]
    fn parse_from_full_config_ignores_other_keys() {
        let text = r#"
model = "qwen-plus"
provider = "openai"

[[backends]]
name = "dashscope"
kind = "openai_compatible"
base_url = "https://example.com/v1"
api_key_env = "DASHSCOPE_API_KEY"

[[models]]
alias = "qwen-plus"
serve = [{ backend = "dashscope", model = "qwen3.5-plus" }]
"#;
        let v: toml::Value = text.parse().unwrap();
        let reg = parse_from_config_value(&v);
        assert_eq!(reg.backends.len(), 1);
        assert_eq!(reg.models[0].alias, "qwen-plus");
    }

    #[test]
    fn merge_prefers_overlay_alias() {
        let base = parse_registry_toml(
            r#"
[[models]]
alias = "x"
serve = [{ backend = "a", model = "old" }]
"#,
        )
        .unwrap();
        let over = parse_registry_toml(
            r#"
[[models]]
alias = "x"
serve = [{ backend = "b", model = "new" }]
"#,
        )
        .unwrap();
        let m = merge(base, over);
        assert_eq!(m.models.len(), 1);
        assert_eq!(m.models[0].serve[0].model, "new");
    }
}
