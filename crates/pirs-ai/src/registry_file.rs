//! Shared multi-backend model registry (user `~/.pirs/config.toml` shape).
//!
//! Used by both the `pirs` harness (with project-trust layering on top) and
//! `pirs-claw` (user layer only).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::anyhow;
use serde::Deserialize;

use crate::{
    AnthropicClient, BackendKind, LlmProvider, ModelRoute, OpenAiCompat, RoutingProvider,
    ServeTarget,
};

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
    #[serde(default)]
    pub persona: Option<String>,
    pub tier: Option<String>,
    pub ctx: Option<u64>,
    #[serde(default)]
    pub caps: Vec<String>,
    #[serde(default)]
    pub serve: Vec<ServeEntry>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct ServeEntry {
    pub backend: String,
    pub model: String,
}

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

pub fn parse_from_config_value(value: &toml::Value) -> RegistryFile {
    let Some(table) = value.as_table() else {
        return RegistryFile::default();
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
            eprintln!("[registry: parse warning: {e}]");
            RegistryFile::default()
        }
    }
}

pub fn user_config_path() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".pirs").join("config.toml"))
}

/// Load **user** `~/.pirs/config.toml` only (claw + harness user layer).
pub fn load_user_registry() -> RegistryFile {
    let Some(path) = user_config_path() else {
        return RegistryFile::default();
    };
    let Ok(text) = std::fs::read_to_string(&path) else {
        return RegistryFile::default();
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return RegistryFile::default();
    };
    parse_from_config_value(&v)
}

pub fn api_key_for_alias(registry: &RegistryFile, alias: &str) -> Option<String> {
    let model = registry.models.iter().find(|m| m.alias == alias)?;
    let serve = model.serve.first()?;
    let backend = registry.backends.iter().find(|b| b.name == serve.backend)?;
    let env = backend.api_key_env.as_ref()?;
    std::env::var(env).ok().filter(|s| !s.is_empty())
}

pub fn first_available_backend_key(registry: &RegistryFile) -> Option<String> {
    for b in &registry.backends {
        if let Some(env) = &b.api_key_env {
            if let Ok(k) = std::env::var(env) {
                if !k.is_empty() {
                    return Some(k);
                }
            }
        }
    }
    None
}

pub fn expected_key_envs(registry: &RegistryFile) -> Vec<String> {
    let mut v: Vec<String> = registry
        .backends
        .iter()
        .filter_map(|b| b.api_key_env.clone())
        .collect();
    v.sort();
    v.dedup();
    v
}

pub fn build_routing_provider(
    registry: &RegistryFile,
    default: Arc<dyn LlmProvider>,
    default_api_key: Option<String>,
    max_retries: u32,
) -> anyhow::Result<Option<Arc<RoutingProvider>>> {
    // Always build a router when backends exist so pin `backend/model` works
    // even without portable [[models]] entries.
    if registry.backends.is_empty() && registry.models.is_empty() {
        return Ok(None);
    }
    type BackendHandle = (Arc<dyn LlmProvider>, Option<String>, Vec<(String, String)>);
    let mut backend_handles: HashMap<String, BackendHandle> = HashMap::new();

    for b in &registry.backends {
        let kind = BackendKind::parse(&b.kind)
            .ok_or_else(|| anyhow!("backend {:?}: unknown kind {:?}", b.name, b.kind))?;
        let api_key = b
            .api_key_env
            .as_ref()
            .and_then(|env| std::env::var(env).ok())
            .filter(|s| !s.is_empty());
        // Only warn for backends the user explicitly configured beyond builtins
        // is hard to know; warn only when key env is set-name but empty and
        // they have at least one other key (noise reduction for full builtin set).
        let headers: Vec<(String, String)> =
            b.headers.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
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
        if m.serve.is_empty() {
            // Skip empty rather than hard-fail (partial user configs).
            continue;
        }
        let mut serve = Vec::new();
        for s in &m.serve {
            if !backend_handles.contains_key(&s.backend) && s.backend != "default" {
                // Skip unknown backends in serve list (e.g. typo) with a note.
                eprintln!(
                    "[model registry: portable {:?} skips unknown backend {:?}]",
                    m.alias, s.backend
                );
                continue;
            }
            serve.push(ServeTarget {
                backend: s.backend.clone(),
                remote_model: s.model.clone(),
            });
        }
        if serve.is_empty() {
            continue;
        }
        routes.push(ModelRoute {
            alias: m.alias.clone(),
            serve,
            tier: m.tier.clone(),
            ctx: m.ctx,
        });
    }

    if routes
        .iter()
        .any(|r| r.serve.iter().any(|s| s.backend == "default"))
    {
        backend_handles.insert(
            "default".into(),
            (Arc::clone(&default), default_api_key.clone(), vec![]),
        );
    }

    // Always register default handle for legacy bare-name fallback.
    if !backend_handles.contains_key("default") {
        backend_handles.insert(
            "default".into(),
            (Arc::clone(&default), default_api_key.clone(), vec![]),
        );
    }

    Ok(Some(Arc::new(RoutingProvider::new(
        default,
        default_api_key,
        vec![],
        backend_handles,
        routes,
    ))))
}

/// Whether a config path has models (tests / diagnostics).
pub fn registry_file_has_models(path: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let Ok(v) = text.parse::<toml::Value>() else {
        return false;
    };
    !parse_from_config_value(&v).models.is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_sample_registry() {
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
        assert_eq!(reg.models.len(), 1);
        assert_eq!(reg.models[0].alias, "qwen3.5-plus");
    }

    #[test]
    fn merge_overrides_alias() {
        let a = parse_from_config_value(
            &r#"
[[backends]]
name = "b"
kind = "openai_compatible"
base_url = "https://a.example/v1"
[[models]]
alias = "m"
serve = [{ backend = "b", model = "old" }]
"#
            .parse()
            .unwrap(),
        );
        let b = parse_from_config_value(
            &r#"
[[models]]
alias = "m"
serve = [{ backend = "b", model = "new" }]
"#
            .parse()
            .unwrap(),
        );
        let m = merge(a, b);
        assert_eq!(m.models.len(), 1);
        assert_eq!(m.models[0].serve[0].model, "new");
        assert_eq!(m.backends.len(), 1);
    }
}
