//! Layered file-based config: `.pirs/config.toml` (project, nearest ancestor
//! of cwd wins) sits above `~/.pirs/config.toml` (user), both below whatever
//! clap already resolved from the CLI or an env var. This only fills in the
//! handful of settings people actually want to pin per-project or per-machine
//! without retyping a flag every run — it does not replace clap, and it never
//! wins against something the user actually typed or exported.
//!
//! Precedence, highest to lowest: CLI flag > env var > project config > user
//! config > hardcoded default. Every resolved value is tagged with exactly
//! which layer won, so `--show-config` can answer "why is this set to X"
//! instead of the user grepping through env vars and three config files.

use std::path::{Path, PathBuf};

use clap::parser::ValueSource;
use clap::ArgMatches;
use serde::Deserialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfigSource {
    Cli,
    Env,
    ProjectConfig,
    UserConfig,
    Default,
}

impl ConfigSource {
    pub fn label(self) -> &'static str {
        match self {
            ConfigSource::Cli => "cli flag",
            ConfigSource::Env => "env var",
            ConfigSource::ProjectConfig => "project config",
            ConfigSource::UserConfig => "user config",
            ConfigSource::Default => "default",
        }
    }
}

/// The subset of settings a `config.toml` layer may set. Deliberately a small,
/// hand-picked list (not "every clap flag") — these are the ones worth pinning
/// per-project or per-machine; everything else stays flag/env-only.
///
/// `base_url` and `approval` are parsed here for both layers (so a malformed
/// project file is still validated/warned about like any other), but the
/// caller in `main.rs` deliberately never applies them from the *project*
/// layer specifically — only from the user layer. A cloned, untrusted repo's
/// own `.pirs/config.toml` must not be able to silently redirect API traffic
/// (`base_url`) or turn off the approval gate (`approval`) just by being
/// checked out and run. `model`/`provider` carry no such risk and stay
/// project-configurable.
#[derive(Debug, Default, Deserialize)]
pub struct FileConfig {
    pub model: Option<String>,
    pub provider: Option<String>,
    pub base_url: Option<String>,
    pub approval: Option<String>,
}

/// Expand `!command` (run it, use trimmed stdout — e.g. a password-manager
/// CLI or `gh auth token`) and `${VAR}`/`$$` in one optional config value.
/// Reuses pirs-mcp's config-value interpolator (already used for `.mcp.json`
/// headers/url/args/env) rather than a second implementation of the same
/// mini-DSL.
fn interpolate_opt(v: Option<String>) -> Option<String> {
    v.map(|s| pirs_mcp::config::interpolate(&s))
}

/// Load one TOML layer. A missing file is silent (that layer just isn't
/// present); a malformed file is a loud warning, not a crash — a typo in
/// `~/.pirs/config.toml` must never stop the CLI from starting at all.
pub fn load_layer(path: &Path) -> FileConfig {
    let Ok(text) = std::fs::read_to_string(path) else {
        return FileConfig::default();
    };
    match toml::from_str::<FileConfig>(&text) {
        Ok(cfg) => FileConfig {
            model: interpolate_opt(cfg.model),
            provider: interpolate_opt(cfg.provider),
            base_url: interpolate_opt(cfg.base_url),
            approval: interpolate_opt(cfg.approval),
        },
        Err(e) => {
            eprintln!(
                "[warning: {} is malformed, ignoring it: {e}]",
                path.display()
            );
            FileConfig::default()
        }
    }
}

pub fn user_config_path() -> Option<PathBuf> {
    std::env::var("HOME")
        .ok()
        .map(|h| Path::new(&h).join(".pirs").join("config.toml"))
}

/// Walk from `start` up to the filesystem root looking for `.pirs/config.toml`,
/// nearest ancestor wins (mirrors the existing `.pirs/skills`, `.pirs/commands`
/// project-discovery convention elsewhere in this crate).
pub fn find_project_config(start: &Path) -> Option<PathBuf> {
    let mut dir = Some(start);
    while let Some(d) = dir {
        let candidate = d.join(".pirs").join("config.toml");
        if candidate.is_file() {
            return Some(candidate);
        }
        dir = d.parent();
    }
    None
}

/// Load both layers relative to `cwd`, project first (nearer wins over user).
/// The project layer is returned as-loaded (not yet restricted) so a caller
/// can warn if it tried to set a security-relevant field before calling
/// `restrict_project_layer`.
pub fn load_layers(cwd: &Path) -> (FileConfig, FileConfig) {
    let project = find_project_config(cwd)
        .map(|p| load_layer(&p))
        .unwrap_or_default();
    let user = user_config_path()
        .map(|p| load_layer(&p))
        .unwrap_or_default();
    (project, user)
}

/// Strip the fields a project-layer config must never be allowed to set: a
/// `git clone`d repo's own `.pirs/config.toml` must not silently redirect API
/// traffic (`base_url`) or turn off the approval gate (`approval`) just by
/// being checked out and run. Call this on the project layer, after checking
/// whether to warn, before passing it to any `resolve_*` call — that way
/// there's one enforcement point rather than relying on every call site
/// remembering to pass `None` for these two fields.
pub fn restrict_project_layer(mut cfg: FileConfig) -> FileConfig {
    cfg.base_url = None;
    cfg.approval = None;
    cfg
}

/// Resolve one `String`-with-a-clap-default field. `arg_id` must match the
/// clap `#[arg]`'s field name (clap derive uses the field name as the id).
pub fn resolve_str(
    matches: &ArgMatches,
    arg_id: &str,
    current: &str,
    project: Option<&str>,
    user: Option<&str>,
) -> (String, ConfigSource) {
    match matches.value_source(arg_id) {
        Some(ValueSource::CommandLine) => (current.to_string(), ConfigSource::Cli),
        Some(ValueSource::EnvVariable) => (current.to_string(), ConfigSource::Env),
        _ => {
            if let Some(v) = project {
                (v.to_string(), ConfigSource::ProjectConfig)
            } else if let Some(v) = user {
                (v.to_string(), ConfigSource::UserConfig)
            } else {
                (current.to_string(), ConfigSource::Default)
            }
        }
    }
}

/// Resolve one `Option<String>` field (no clap default — `None` means neither
/// CLI nor env gave it, so config-file layers get a real say).
pub fn resolve_opt(
    matches: &ArgMatches,
    arg_id: &str,
    current: Option<String>,
    project: Option<&str>,
    user: Option<&str>,
) -> (Option<String>, ConfigSource) {
    match matches.value_source(arg_id) {
        Some(ValueSource::CommandLine) => (current, ConfigSource::Cli),
        Some(ValueSource::EnvVariable) => (current, ConfigSource::Env),
        _ => {
            if let Some(v) = project {
                (Some(v.to_string()), ConfigSource::ProjectConfig)
            } else if let Some(v) = user {
                (Some(v.to_string()), ConfigSource::UserConfig)
            } else {
                (current, ConfigSource::Default)
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn load_layer_interpolates_env_vars_and_shell_commands() {
        std::env::set_var("PIRS_CONFIG_TEST_PROVIDER", "openai-from-env");
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(
            &path,
            "model = \"!echo model-from-shell\"\nprovider = \"${PIRS_CONFIG_TEST_PROVIDER}\"\n",
        )
        .unwrap();
        let cfg = load_layer(&path);
        assert_eq!(cfg.model.as_deref(), Some("model-from-shell"));
        assert_eq!(cfg.provider.as_deref(), Some("openai-from-env"));
    }

    #[test]
    fn restrict_project_layer_strips_security_fields_but_keeps_preferences() {
        let cfg = FileConfig {
            model: Some("gpt-5-mini".into()),
            provider: Some("openai".into()),
            base_url: Some("https://attacker.example.com/v1".into()),
            approval: Some("yolo".into()),
        };
        let restricted = restrict_project_layer(cfg);
        assert_eq!(restricted.model.as_deref(), Some("gpt-5-mini"));
        assert_eq!(restricted.provider.as_deref(), Some("openai"));
        assert_eq!(
            restricted.base_url, None,
            "a project config must never set base_url"
        );
        assert_eq!(
            restricted.approval, None,
            "a project config must never set approval"
        );
    }

    #[test]
    fn malformed_config_warns_and_falls_back_to_default() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("config.toml");
        std::fs::write(&path, "model = [this is not valid toml").unwrap();
        let cfg = load_layer(&path);
        assert!(cfg.model.is_none());
    }

    #[test]
    fn missing_config_is_silent_default() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = load_layer(&dir.path().join("nope.toml"));
        assert!(cfg.model.is_none());
    }

    #[test]
    fn project_config_found_from_nested_subdir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".pirs")).unwrap();
        std::fs::write(
            root.join(".pirs").join("config.toml"),
            "model = \"from-project\"\n",
        )
        .unwrap();
        let nested = root.join("a").join("b");
        std::fs::create_dir_all(&nested).unwrap();

        let found = find_project_config(&nested).unwrap();
        let cfg = load_layer(&found);
        assert_eq!(cfg.model.as_deref(), Some("from-project"));
    }

    #[test]
    fn nearest_project_config_wins_over_a_further_ancestor() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join(".pirs")).unwrap();
        std::fs::write(root.join(".pirs").join("config.toml"), "model = \"far\"\n").unwrap();
        let nested = root.join("nested");
        std::fs::create_dir_all(nested.join(".pirs")).unwrap();
        std::fs::write(
            nested.join(".pirs").join("config.toml"),
            "model = \"near\"\n",
        )
        .unwrap();

        let found = find_project_config(&nested).unwrap();
        let cfg = load_layer(&found);
        assert_eq!(cfg.model.as_deref(), Some("near"));
    }
}
