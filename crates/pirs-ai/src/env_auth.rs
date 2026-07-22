//! Resolve OpenAI-compatible base URL + API key from well-known environment
//! variables (after `secrets.env` load).
//!
//! Used when the multi-backend registry is empty or does not cover the model.
//! Model-aware preference avoids routing `deepseek-*` to DashScope just because
//! `DASHSCOPE_API_KEY` is also set (that misroute yields empty assistant text).

/// Non-empty env var value, if set.
pub fn non_empty_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|s| !s.is_empty())
}

/// Well-known key env names (for error hints).
pub fn well_known_key_envs() -> &'static [&'static str] {
    &[
        "OPENAI_API_KEY",
        "DASHSCOPE_API_KEY",
        "DEEPSEEK_API_KEY",
        "OPENROUTER_API_KEY",
        "ANTHROPIC_API_KEY",
        "GROQ_API_KEY",
    ]
}

const DASHSCOPE_BASE: &str = "https://coding-intl.dashscope.aliyuncs.com/v1";
const DEEPSEEK_BASE: &str = "https://api.deepseek.com/v1";
const OPENROUTER_BASE: &str = "https://openrouter.ai/api/v1";

/// Pick OpenAI-compatible `(base_url, api_key)` from the environment.
///
/// When `model_hint` looks like a DeepSeek or Qwen id, prefer the matching
/// provider key even if another key is also set.
pub fn resolve_openai_compat(model_hint: Option<&str>) -> (Option<String>, Option<String>) {
    if let Some(base) = non_empty_env("OPENAI_BASE_URL") {
        let key = non_empty_env("OPENAI_API_KEY")
            .or_else(|| non_empty_env("DASHSCOPE_API_KEY"))
            .or_else(|| non_empty_env("DEEPSEEK_API_KEY"))
            .or_else(|| non_empty_env("OPENROUTER_API_KEY"));
        return (Some(base), key);
    }

    let model = model_hint.unwrap_or("").to_ascii_lowercase();

    if model_looks_deepseek(&model) {
        if let Some(k) = non_empty_env("DEEPSEEK_API_KEY") {
            return (Some(DEEPSEEK_BASE.into()), Some(k));
        }
    }
    if model_looks_qwen(&model) {
        if let Some(k) = non_empty_env("DASHSCOPE_API_KEY") {
            return (Some(DASHSCOPE_BASE.into()), Some(k));
        }
    }
    if model_looks_openrouter(&model) {
        if let Some(k) = non_empty_env("OPENROUTER_API_KEY") {
            return (Some(OPENROUTER_BASE.into()), Some(k));
        }
    }

    // Generic priority when the model name does not imply a backend.
    if let Some(k) = non_empty_env("DASHSCOPE_API_KEY") {
        return (Some(DASHSCOPE_BASE.into()), Some(k));
    }
    if let Some(k) = non_empty_env("DEEPSEEK_API_KEY") {
        return (Some(DEEPSEEK_BASE.into()), Some(k));
    }
    if let Some(k) = non_empty_env("OPENROUTER_API_KEY") {
        return (Some(OPENROUTER_BASE.into()), Some(k));
    }
    (None, non_empty_env("OPENAI_API_KEY"))
}

fn model_looks_deepseek(model: &str) -> bool {
    model.contains("deepseek")
}

fn model_looks_qwen(model: &str) -> bool {
    model.contains("qwen") || model.contains("qwq") || model.starts_with("qwen")
}

fn model_looks_openrouter(model: &str) -> bool {
    model.contains('/') // openrouter style org/model
        || model.contains("openrouter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    static LOCK: Mutex<()> = Mutex::new(());

    fn clear_keys() {
        for k in [
            "OPENAI_BASE_URL",
            "OPENAI_API_KEY",
            "DASHSCOPE_API_KEY",
            "DEEPSEEK_API_KEY",
            "OPENROUTER_API_KEY",
        ] {
            std::env::remove_var(k);
        }
    }

    #[test]
    fn deepseek_model_prefers_deepseek_key_over_dashscope() {
        let _g = LOCK.lock().unwrap();
        clear_keys();
        std::env::set_var("DASHSCOPE_API_KEY", "dscope");
        std::env::set_var("DEEPSEEK_API_KEY", "dseek");
        let (base, key) = resolve_openai_compat(Some("deepseek-v4-flash"));
        assert_eq!(base.as_deref(), Some(DEEPSEEK_BASE));
        assert_eq!(key.as_deref(), Some("dseek"));
        clear_keys();
    }

    #[test]
    fn qwen_model_prefers_dashscope() {
        let _g = LOCK.lock().unwrap();
        clear_keys();
        std::env::set_var("DASHSCOPE_API_KEY", "dscope");
        std::env::set_var("DEEPSEEK_API_KEY", "dseek");
        let (base, key) = resolve_openai_compat(Some("qwen3.5-plus"));
        assert_eq!(base.as_deref(), Some(DASHSCOPE_BASE));
        assert_eq!(key.as_deref(), Some("dscope"));
        clear_keys();
    }

    #[test]
    fn generic_prefers_dashscope_when_both_set() {
        let _g = LOCK.lock().unwrap();
        clear_keys();
        std::env::set_var("DASHSCOPE_API_KEY", "dscope");
        std::env::set_var("DEEPSEEK_API_KEY", "dseek");
        let (base, key) = resolve_openai_compat(Some("gpt-4o-mini"));
        assert_eq!(base.as_deref(), Some(DASHSCOPE_BASE));
        assert_eq!(key.as_deref(), Some("dscope"));
        clear_keys();
    }

    #[test]
    fn empty_env_values_ignored() {
        let _g = LOCK.lock().unwrap();
        clear_keys();
        std::env::set_var("OPENAI_API_KEY", "");
        std::env::set_var("DEEPSEEK_API_KEY", "dseek");
        let (base, key) = resolve_openai_compat(Some("deepseek-v4-pro"));
        assert_eq!(base.as_deref(), Some(DEEPSEEK_BASE));
        assert_eq!(key.as_deref(), Some("dseek"));
        clear_keys();
    }
}
