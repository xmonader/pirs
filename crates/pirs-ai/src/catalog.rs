//! Per-backend model catalog: fetch, cache, search, refresh.
//!
//! Cache lives under `~/.pirs/cache/catalogs/<backend>.json`.

use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{anyhow, bail, Context as _};
use serde::{Deserialize, Serialize};

use crate::registry_file::{BackendEntry, RegistryFile};
use crate::routing::BackendKind;

const DEFAULT_TTL_SECS: u64 = 86_400; // 24h

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogModel {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ctx: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogFile {
    pub backend: String,
    /// Unix seconds when fetched.
    pub fetched_at: u64,
    pub ttl_secs: u64,
    pub models: Vec<CatalogModel>,
}

impl CatalogFile {
    pub fn is_stale(&self) -> bool {
        let now = now_secs();
        now.saturating_sub(self.fetched_at) > self.ttl_secs
    }

    pub fn age_secs(&self) -> u64 {
        now_secs().saturating_sub(self.fetched_at)
    }
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn catalog_cache_dir() -> Option<PathBuf> {
    let home = std::env::var_os("HOME")?;
    Some(PathBuf::from(home).join(".pirs").join("cache").join("catalogs"))
}

pub fn catalog_path(backend: &str) -> Option<PathBuf> {
    // Sanitize name for filesystem.
    let safe: String = backend
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();
    Some(catalog_cache_dir()?.join(format!("{safe}.json")))
}

pub fn load_catalog(backend: &str) -> Option<CatalogFile> {
    let path = catalog_path(backend)?;
    let text = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&text).ok()
}

pub fn save_catalog(cat: &CatalogFile) -> anyhow::Result<PathBuf> {
    let path = catalog_path(&cat.backend).context("HOME unset; cannot write catalog cache")?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let text = serde_json::to_string_pretty(cat)?;
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, &path)?;
    Ok(path)
}

pub fn ttl_secs() -> u64 {
    std::env::var("PIRS_CATALOG_TTL")
        .ok()
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_TTL_SECS)
}

/// Fetch models from a backend's catalog API (blocking HTTP).
pub fn fetch_catalog(backend: &BackendEntry) -> anyhow::Result<CatalogFile> {
    let kind = BackendKind::parse(&backend.kind)
        .ok_or_else(|| anyhow!("unknown backend kind {:?}", backend.kind))?;
    let api_key = backend
        .api_key_env
        .as_ref()
        .and_then(|e| std::env::var(e).ok())
        .filter(|s| !s.is_empty());

    let models = match kind {
        BackendKind::OpenaiCompatible => fetch_openai_compatible_models(backend, api_key.as_deref())?,
        BackendKind::Anthropic => anthropic_static_models(),
    };

    Ok(CatalogFile {
        backend: backend.name.clone(),
        fetched_at: now_secs(),
        ttl_secs: ttl_secs(),
        models,
    })
}

fn fetch_openai_compatible_models(
    backend: &BackendEntry,
    api_key: Option<&str>,
) -> anyhow::Result<Vec<CatalogModel>> {
    let base = backend.base_url.trim_end_matches('/');
    let url = format!("{base}/models");
    let client = reqwest::blocking::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .context("http client")?;
    let mut req = client.get(&url);
    if let Some(key) = api_key {
        req = req.bearer_auth(key);
    }
    for (k, v) in &backend.headers {
        req = req.header(k, v);
    }
    // OpenRouter sometimes wants Authorization only; fine.
    let resp = req.send().with_context(|| format!("GET {url}"))?;
    let status = resp.status();
    let body = resp.text().unwrap_or_default();
    if !status.is_success() {
        bail!(
            "catalog {} HTTP {status}: {}",
            backend.name,
            body.chars().take(200).collect::<String>()
        );
    }
    parse_openai_models_json(&body)
}

fn parse_openai_models_json(body: &str) -> anyhow::Result<Vec<CatalogModel>> {
    let v: serde_json::Value = serde_json::from_str(body).context("catalog JSON")?;
    let data = v
        .get("data")
        .and_then(|d| d.as_array())
        .or_else(|| v.as_array())
        .ok_or_else(|| anyhow!("catalog JSON missing data[]"))?;
    let mut models = Vec::with_capacity(data.len());
    for item in data {
        let id = item
            .get("id")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            continue;
        }
        let name = item
            .get("name")
            .or_else(|| item.get("display_name"))
            .and_then(|x| x.as_str())
            .map(|s| s.to_string());
        let ctx = item
            .get("context_length")
            .or_else(|| item.get("context_window"))
            .and_then(|x| x.as_u64())
            .or_else(|| {
                item.get("top_provider")
                    .and_then(|t| t.get("context_length"))
                    .and_then(|x| x.as_u64())
            });
        models.push(CatalogModel { id, name, ctx });
    }
    models.sort_by(|a, b| a.id.cmp(&b.id));
    models.dedup_by(|a, b| a.id == b.id);
    Ok(models)
}

fn anthropic_static_models() -> Vec<CatalogModel> {
    // Anthropic has no stable public /models for all API keys; ship a short list.
    [
        "claude-sonnet-4-5",
        "claude-opus-4-5",
        "claude-haiku-4-5",
        "claude-3-5-sonnet-latest",
        "claude-3-5-haiku-latest",
    ]
    .into_iter()
    .map(|id| CatalogModel {
        id: id.into(),
        name: Some(id.into()),
        ctx: None,
    })
    .collect()
}

/// Refresh one backend by name; returns catalog + path written.
pub fn refresh_backend(reg: &RegistryFile, name: &str) -> anyhow::Result<(CatalogFile, PathBuf)> {
    let backend = reg
        .backends
        .iter()
        .find(|b| b.name == name)
        .ok_or_else(|| {
            anyhow!(
                "unknown backend {name:?}; known: {}",
                reg.backends
                    .iter()
                    .map(|b| b.name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            )
        })?;
    if let Some(env) = &backend.api_key_env {
        if std::env::var(env).ok().filter(|s| !s.is_empty()).is_none() {
            bail!(
                "backend {name:?} needs {env} set (env or ~/.pirs/secrets.env) before refresh"
            );
        }
    }
    let cat = fetch_catalog(backend)?;
    let path = save_catalog(&cat)?;
    Ok((cat, path))
}

/// Refresh all backends that currently have keys.
pub fn refresh_active(reg: &RegistryFile) -> Vec<(String, Result<CatalogFile, String>)> {
    let mut out = Vec::new();
    for b in &reg.backends {
        let has_key = match &b.api_key_env {
            None => true,
            Some(e) => std::env::var(e).ok().filter(|s| !s.is_empty()).is_some(),
        };
        if !has_key {
            continue;
        }
        match fetch_catalog(b).and_then(|c| {
            save_catalog(&c)?;
            Ok(c)
        }) {
            Ok(c) => out.push((b.name.clone(), Ok(c))),
            Err(e) => out.push((b.name.clone(), Err(e.to_string()))),
        }
    }
    out
}

/// Search cached catalogs (and optional live if missing) for a substring.
pub fn search_catalogs(reg: &RegistryFile, query: &str) -> Vec<(String, CatalogModel)> {
    let q = query.to_ascii_lowercase();
    let mut hits = Vec::new();
    for b in &reg.backends {
        let Some(cat) = load_catalog(&b.name) else {
            continue;
        };
        for m in cat.models {
            if m.id.to_ascii_lowercase().contains(&q)
                || m.name
                    .as_ref()
                    .is_some_and(|n| n.to_ascii_lowercase().contains(&q))
            {
                hits.push((b.name.clone(), m));
            }
        }
    }
    hits.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.id.cmp(&b.1.id)));
    hits
}

/// Load cache or empty; used by status printers.
pub fn catalog_status(backend: &str) -> Option<(usize, u64, bool)> {
    let cat = load_catalog(backend)?;
    Some((cat.models.len(), cat.age_secs(), cat.is_stale()))
}

/// Ensure cache dir exists (tests / setup).
pub fn ensure_cache_dir() -> anyhow::Result<PathBuf> {
    let dir = catalog_cache_dir().context("HOME unset")?;
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_openai_style_catalog() {
        let body = r#"{"data":[{"id":"a/b","name":"AB","context_length":100},{"id":"c"}]}"#;
        let m = parse_openai_models_json(body).unwrap();
        assert_eq!(m.len(), 2);
        assert_eq!(m[0].id, "a/b");
        assert_eq!(m[0].ctx, Some(100));
    }
}
