//! Named backends + model aliases — shared parse/build in `pirs_ai::registry_file`.
//!
//! This module adds **project trust layering** and secrets.env loading for the harness.

use std::path::Path;
use std::sync::Arc;

use pirs_ai::{
    api_key_for_alias as shared_api_key, build_routing_provider as shared_build,
    expected_key_envs as shared_envs, first_available_backend_key as shared_first_key,
    merge_registry, parse_from_config_value as shared_parse, LlmProvider, RoutingProvider,
};

pub use pirs_ai::RegistryFile;

pub fn merge(base: RegistryFile, over: RegistryFile) -> RegistryFile {
    merge_registry(base, over)
}

#[cfg(test)]
pub fn parse_registry_toml(text: &str) -> anyhow::Result<RegistryFile> {
    let v: toml::Value = text.parse()?;
    Ok(shared_parse(&v))
}

pub fn parse_from_config_value(value: &toml::Value) -> RegistryFile {
    shared_parse(value)
}

/// True when the project's `.pirs/extensions` is in the pirs trust store.
fn project_is_trusted(project_pirs_dir: &Path) -> bool {
    let Ok(home) = std::env::var("HOME") else {
        return false;
    };
    let path = Path::new(&home).join(".pirs").join("trusted.json");
    let Ok(text) = std::fs::read_to_string(path) else {
        return false;
    };
    let set: std::collections::HashSet<String> = serde_json::from_str(&text).unwrap_or_default();
    if set.is_empty() {
        return false;
    }
    let ext = project_pirs_dir.join("extensions");
    let ext = ext.canonicalize().unwrap_or(ext);
    let prefix = format!("{}#", ext.display());
    let path_s = ext.to_string_lossy();
    set.iter()
        .any(|t| t.starts_with(&prefix) || t == path_s.as_ref() || t.starts_with(path_s.as_ref()))
}

/// Load `~/.pirs/secrets.env` into the process environment for any vars not already set.
pub fn load_secrets_env() {
    let Some(home) = std::env::var_os("HOME") else {
        return;
    };
    let path = Path::new(&home).join(".pirs").join("secrets.env");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return;
    };
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let body = line.strip_prefix("export ").unwrap_or(line).trim();
        let Some((name, raw)) = body.split_once('=') else {
            continue;
        };
        let name = name.trim();
        if name.is_empty() || std::env::var_os(name).is_some() {
            continue;
        }
        let mut val = raw.trim().to_string();
        if (val.starts_with('\'') && val.ends_with('\''))
            || (val.starts_with('"') && val.ends_with('"'))
        {
            val = val[1..val.len() - 1].to_string();
        }
        if val.starts_with("${") && val.ends_with('}') {
            let ref_name = &val[2..val.len() - 1];
            val = std::env::var(ref_name).unwrap_or_default();
            if val.is_empty() {
                continue;
            }
        }
        std::env::set_var(name, val);
    }
}

pub fn api_key_for_alias(registry: &RegistryFile, alias: &str) -> Option<String> {
    shared_api_key(registry, alias)
}

pub fn first_available_backend_key(registry: &RegistryFile) -> Option<String> {
    shared_first_key(registry)
}

pub fn expected_key_envs(registry: &RegistryFile) -> Vec<String> {
    shared_envs(registry)
}

pub fn load_registry_layers(cwd: &Path) -> RegistryFile {
    // Lowest layer: builtin backends + curated portable models.
    let mut reg = pirs_ai::builtin_registry();
    if let Some(path) = crate::config_file::user_config_path() {
        if let Ok(text) = std::fs::read_to_string(&path) {
            if let Ok(v) = text.parse::<toml::Value>() {
                reg = merge(reg, parse_from_config_value(&v));
            }
        }
    }
    if let Some(path) = crate::config_file::find_project_config(cwd) {
        let user_path = crate::config_file::user_config_path();
        let same_as_user = user_path
            .as_ref()
            .and_then(|u| {
                let a = path.canonicalize().ok()?;
                let b = u.canonicalize().ok()?;
                Some(a == b)
            })
            .unwrap_or(false);
        if !same_as_user {
            if let Ok(text) = std::fs::read_to_string(&path) {
                if let Ok(v) = text.parse::<toml::Value>() {
                    let mut project = parse_from_config_value(&v);
                    if !project.backends.is_empty() {
                        let project_dir = path.parent().unwrap_or(Path::new("."));
                        if project_is_trusted(project_dir) {
                            eprintln!(
                                "[model registry: loading {} backend(s) from trusted project config {}]",
                                project.backends.len(),
                                path.display()
                            );
                        } else {
                            eprintln!(
                                "[note: project {} defines backends but is not trusted — \
                                 backends ignored (run `pirs trust` in the project, or move \
                                 backends to ~/.pirs/config.toml). Model aliases still load.]",
                                path.display()
                            );
                            project.backends.clear();
                        }
                    }
                    reg = merge(reg, project);
                }
            }
        }
    }
    reg
}

pub fn build_routing_provider(
    registry: &RegistryFile,
    default: Arc<dyn LlmProvider>,
    default_api_key: Option<String>,
    max_retries: u32,
) -> anyhow::Result<Option<Arc<RoutingProvider>>> {
    shared_build(registry, default, default_api_key, max_retries)
}

#[cfg(test)]
mod tests {
    use super::*;
    use pirs_ai::OpenAiCompat;

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
serve = [
  { backend = "openrouter", model = "deepseek/deepseek-v4-flash" },
  { backend = "dashscope", model = "deepseek-v4-flash" },
]

[[models]]
alias = "qwen-plus"
serve = [{ backend = "dashscope", model = "qwen3.5-plus" }]
"#;

    #[test]
    fn parses_backends_and_failover_serve_list() {
        let reg = parse_registry_toml(SAMPLE).unwrap();
        assert_eq!(reg.backends.len(), 2);
        let m = reg
            .models
            .iter()
            .find(|m| m.alias == "deepseek-v4-flash")
            .unwrap();
        assert_eq!(m.serve.len(), 2);
        assert_eq!(m.serve[0].backend, "openrouter");
        assert_eq!(m.serve[1].backend, "dashscope");
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

    #[test]
    fn build_routing_keeps_full_serve_chain() {
        let reg = parse_registry_toml(SAMPLE).unwrap();
        let default: Arc<dyn LlmProvider> = Arc::new(OpenAiCompat::new(None));
        let router = build_routing_provider(&reg, default, None, 0)
            .unwrap()
            .expect("router");
        let targets = router.targets_for("deepseek-v4-flash");
        assert_eq!(targets.len(), 2);
        assert_eq!(targets[0].backend_name, "openrouter");
        assert_eq!(targets[1].backend_name, "dashscope");
    }
}
