//! Model specification: **pin** (`backend/remote`) vs **portable** (bare name).
//!
//! Parse rule for pins: split on the **first** `/` only so OpenRouter-style
//! remote ids (`deepseek/deepseek-v4-flash`) work as
//! `openrouter/deepseek/deepseek-v4-flash`.

use crate::routing::ServeTarget;

/// What the user typed as `--model` / `/model` / plan-model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelSpec {
    /// Force one subscription: `backend` + remote id (may contain `/`).
    Pin {
        backend: String,
        remote: String,
    },
    /// Logical model name; resolved via portable index / aliases.
    Portable(String),
}

impl ModelSpec {
    /// Parse a user model string.
    pub fn parse(s: &str) -> Self {
        let s = s.trim();
        if s.is_empty() {
            return ModelSpec::Portable(String::new());
        }
        // Optional explicit prefixes (future-proof; slash rule is enough).
        if let Some(rest) = s.strip_prefix("pin:") {
            return ModelSpec::parse(rest);
        }
        if let Some(rest) = s.strip_prefix("any:") {
            return ModelSpec::Portable(rest.trim().to_string());
        }
        if let Some((backend, remote)) = s.split_once('/') {
            let backend = backend.trim();
            let remote = remote.trim();
            if !backend.is_empty() && !remote.is_empty() {
                return ModelSpec::Pin {
                    backend: backend.to_string(),
                    remote: remote.to_string(),
                };
            }
        }
        ModelSpec::Portable(s.to_string())
    }

    pub fn is_pin(&self) -> bool {
        matches!(self, ModelSpec::Pin { .. })
    }

    pub fn display(&self) -> String {
        match self {
            ModelSpec::Pin { backend, remote } => format!("{backend}/{remote}"),
            ModelSpec::Portable(n) => n.clone(),
        }
    }

    /// Pin → single serve target; portable is not a serve target by itself.
    pub fn as_pin_target(&self) -> Option<ServeTarget> {
        match self {
            ModelSpec::Pin { backend, remote } => Some(ServeTarget {
                backend: backend.clone(),
                remote_model: remote.clone(),
            }),
            ModelSpec::Portable(_) => None,
        }
    }
}

/// Parse `backend/remote` or reject.
pub fn parse_pin(s: &str) -> Option<ServeTarget> {
    match ModelSpec::parse(s) {
        ModelSpec::Pin { backend, remote } => Some(ServeTarget {
            backend,
            remote_model: remote,
        }),
        ModelSpec::Portable(_) => None,
    }
}

/// Format a serve target as a pin string.
pub fn format_pin(backend: &str, remote: &str) -> String {
    format!("{backend}/{remote}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pin_splits_on_first_slash_only() {
        let s = ModelSpec::parse("openrouter/deepseek/deepseek-v4-flash");
        assert_eq!(
            s,
            ModelSpec::Pin {
                backend: "openrouter".into(),
                remote: "deepseek/deepseek-v4-flash".into(),
            }
        );
    }

    #[test]
    fn portable_has_no_slash() {
        assert_eq!(
            ModelSpec::parse("qwen-plus"),
            ModelSpec::Portable("qwen-plus".into())
        );
    }

    #[test]
    fn dashscope_pin() {
        let s = ModelSpec::parse("dashscope/qwen3.5-plus");
        assert_eq!(
            s,
            ModelSpec::Pin {
                backend: "dashscope".into(),
                remote: "qwen3.5-plus".into(),
            }
        );
    }

    #[test]
    fn empty_remote_falls_to_portable() {
        // "openrouter/" — invalid pin
        assert!(matches!(
            ModelSpec::parse("openrouter/"),
            ModelSpec::Portable(_)
        ));
    }
}
