//! Multi-backend model routing.
//!
//! A [`RoutingProvider`] maps **aliases** (what the user types as `--model` /
//! `--plan-model`) onto concrete backends (base URL + API key + optional headers)
//! and the remote model id that backend expects. Unregistered model names fall
//! through to a default provider unchanged — so plain
//! `--model gpt-4o --provider openai` still works with no registry config.

use std::collections::HashMap;
use std::sync::Arc;

use crate::{CompletionOptions, Context, LlmProvider, StreamEvent};

/// One authenticated API endpoint.
#[derive(Debug, Clone)]
pub struct BackendSpec {
    pub name: String,
    pub kind: BackendKind,
    pub base_url: String,
    /// Env var holding the API key (resolved at build time into `api_key`).
    pub api_key_env: Option<String>,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    OpenaiCompatible,
    Anthropic,
}

impl BackendKind {
    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "openai_compatible" | "openai-compatible" | "openai" => {
                Some(BackendKind::OpenaiCompatible)
            }
            "anthropic" => Some(BackendKind::Anthropic),
            _ => None,
        }
    }
}

/// Alias → first serve target (failover list can be added later).
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub alias: String,
    pub backend: String,
    pub remote_model: String,
    /// Optional metadata (not used for routing; useful for docs/UI later).
    pub tier: Option<String>,
    pub ctx: Option<u64>,
}

/// Built routing table + live providers per backend.
pub struct RoutingProvider {
    routes: HashMap<String, ModelRoute>,
    backends: HashMap<String, BackendHandle>,
    /// Used when `model` is not a registered alias.
    default: BackendHandle,
}

struct BackendHandle {
    provider: Arc<dyn LlmProvider>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    name: String,
}

impl RoutingProvider {
    /// Build a router. `default` is the CLI-constructed provider for unknown
    /// model names; `backends` are named endpoints; `routes` map aliases.
    pub fn new(
        default: Arc<dyn LlmProvider>,
        default_api_key: Option<String>,
        default_headers: Vec<(String, String)>,
        backends: HashMap<String, (Arc<dyn LlmProvider>, Option<String>, Vec<(String, String)>)>,
        routes: Vec<ModelRoute>,
    ) -> Self {
        let mut handles = HashMap::new();
        for (name, (provider, api_key, headers)) in backends {
            handles.insert(
                name.clone(),
                BackendHandle {
                    provider,
                    api_key,
                    headers,
                    name,
                },
            );
        }
        let route_map = routes
            .into_iter()
            .map(|r| (r.alias.clone(), r))
            .collect();
        RoutingProvider {
            routes: route_map,
            backends: handles,
            default: BackendHandle {
                provider: default,
                api_key: default_api_key,
                headers: default_headers,
                name: "default".into(),
            },
        }
    }

    /// Resolve an alias or raw model id into backend + remote model id.
    pub fn resolve(&self, model_or_alias: &str) -> ResolvedRef {
        if let Some(route) = self.routes.get(model_or_alias) {
            if let Some(backend) = self.backends.get(&route.backend) {
                return ResolvedRef {
                    alias: Some(route.alias.clone()),
                    backend_name: backend.name.clone(),
                    remote_model: route.remote_model.clone(),
                    provider: Arc::clone(&backend.provider),
                    api_key: backend.api_key.clone(),
                    headers: backend.headers.clone(),
                };
            }
        }
        ResolvedRef {
            alias: None,
            backend_name: self.default.name.clone(),
            remote_model: model_or_alias.to_string(),
            provider: Arc::clone(&self.default.provider),
            api_key: self.default.api_key.clone(),
            headers: self.default.headers.clone(),
        }
    }

    pub fn has_alias(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    pub fn aliases(&self) -> Vec<&str> {
        let mut v: Vec<_> = self.routes.keys().map(|s| s.as_str()).collect();
        v.sort_unstable();
        v
    }
}

#[derive(Clone)]
pub struct ResolvedRef {
    pub alias: Option<String>,
    pub backend_name: String,
    pub remote_model: String,
    pub provider: Arc<dyn LlmProvider>,
    pub api_key: Option<String>,
    pub headers: Vec<(String, String)>,
}

#[async_trait::async_trait]
impl LlmProvider for RoutingProvider {
    async fn stream(
        &self,
        model: &str,
        context: &Context,
        options: &CompletionOptions,
        cancel: tokio_util::sync::CancellationToken,
    ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
        let resolved = self.resolve(model);
        let mut opts = options.clone();
        // Backend key wins when set (each subscription has its own key).
        if let Some(key) = resolved.api_key.clone() {
            opts.api_key = Some(key);
        }
        // Backend headers first, then any per-call extras (call extras win on
        // duplicate keys only if we append after — last write in reqwest header
        // loop wins if we allow dups; keep backend headers then call).
        let mut headers = resolved.headers;
        headers.extend(opts.extra_headers.iter().cloned());
        opts.extra_headers = headers;

        resolved
            .provider
            .stream(&resolved.remote_model, context, &opts, cancel)
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AssistantMessage, ContentBlock, StopReason};
    use std::sync::Mutex;

    struct CaptureProvider {
        seen: Mutex<Vec<(String, Option<String>, Vec<(String, String)>)>>,
        label: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for CaptureProvider {
        async fn stream(
            &self,
            model: &str,
            _context: &Context,
            options: &CompletionOptions,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
            self.seen.lock().unwrap().push((
                model.to_string(),
                options.api_key.clone(),
                options.extra_headers.clone(),
            ));
            let msg = AssistantMessage {
                content: vec![ContentBlock::text(format!("{}:{}", self.label, model))],
                stop_reason: StopReason::Stop,
                ..Default::default()
            };
            Box::pin(futures_util::stream::iter(vec![StreamEvent::Done(
                Box::new(msg),
            )]))
        }
    }

    #[tokio::test]
    async fn routes_alias_to_backend_remote_model_and_key() {
        let openrouter = Arc::new(CaptureProvider {
            seen: Mutex::new(Vec::new()),
            label: "or".into(),
        });
        let dashscope = Arc::new(CaptureProvider {
            seen: Mutex::new(Vec::new()),
            label: "ds".into(),
        });
        let default = Arc::new(CaptureProvider {
            seen: Mutex::new(Vec::new()),
            label: "def".into(),
        });

        let mut backends = HashMap::new();
        backends.insert(
            "openrouter".into(),
            (
                Arc::clone(&openrouter) as Arc<dyn LlmProvider>,
                Some("or-key".into()),
                vec![("X-Title".into(), "pirs".into())],
            ),
        );
        backends.insert(
            "dashscope".into(),
            (
                Arc::clone(&dashscope) as Arc<dyn LlmProvider>,
                Some("ds-key".into()),
                vec![],
            ),
        );

        let router = RoutingProvider::new(
            Arc::clone(&default) as Arc<dyn LlmProvider>,
            Some("def-key".into()),
            vec![],
            backends,
            vec![
                ModelRoute {
                    alias: "deepseek-v4-flash".into(),
                    backend: "openrouter".into(),
                    remote_model: "deepseek/deepseek-v4-flash".into(),
                    tier: Some("fast".into()),
                    ctx: Some(1_000_000),
                },
                ModelRoute {
                    alias: "qwen-plus".into(),
                    backend: "dashscope".into(),
                    remote_model: "qwen3.5-plus".into(),
                    tier: None,
                    ctx: None,
                },
            ],
        );

        // Strong plan alias → openrouter remote + key
        let _ = router
            .stream(
                "deepseek-v4-flash",
                &Context::default(),
                &CompletionOptions {
                    api_key: Some("cli-key-should-be-overridden".into()),
                    ..Default::default()
                },
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .next()
            .await;

        // Weak exec alias → dashscope
        let _ = router
            .stream(
                "qwen-plus",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .next()
            .await;

        // Unknown → default, model id unchanged
        let _ = router
            .stream(
                "gpt-4o",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .next()
            .await;

        let or = openrouter.seen.lock().unwrap();
        assert_eq!(or.len(), 1);
        assert_eq!(or[0].0, "deepseek/deepseek-v4-flash");
        assert_eq!(or[0].1.as_deref(), Some("or-key"));
        assert!(or[0].2.iter().any(|(k, v)| k == "X-Title" && v == "pirs"));

        let ds = dashscope.seen.lock().unwrap();
        assert_eq!(ds.len(), 1);
        assert_eq!(ds[0].0, "qwen3.5-plus");
        assert_eq!(ds[0].1.as_deref(), Some("ds-key"));

        let def = default.seen.lock().unwrap();
        assert_eq!(def.len(), 1);
        assert_eq!(def[0].0, "gpt-4o");
    }

    /// Simulates `--model qwen-plus --plan-model deepseek-v4-flash`: two sequential
    /// phase calls on the same router must hit different backends/keys.
    #[tokio::test]
    async fn strong_plan_weak_exec_aliases_use_distinct_backends() {
        let openrouter = Arc::new(CaptureProvider {
            seen: Mutex::new(Vec::new()),
            label: "or".into(),
        });
        let dashscope = Arc::new(CaptureProvider {
            seen: Mutex::new(Vec::new()),
            label: "ds".into(),
        });
        let mut backends = HashMap::new();
        backends.insert(
            "openrouter".into(),
            (
                Arc::clone(&openrouter) as Arc<dyn LlmProvider>,
                Some("or-key".into()),
                vec![],
            ),
        );
        backends.insert(
            "dashscope".into(),
            (
                Arc::clone(&dashscope) as Arc<dyn LlmProvider>,
                Some("ds-key".into()),
                vec![],
            ),
        );
        let router = RoutingProvider::new(
            Arc::clone(&dashscope) as Arc<dyn LlmProvider>,
            None,
            vec![],
            backends,
            vec![
                ModelRoute {
                    alias: "deepseek-v4-flash".into(),
                    backend: "openrouter".into(),
                    remote_model: "deepseek/deepseek-v4-flash".into(),
                    tier: None,
                    ctx: None,
                },
                ModelRoute {
                    alias: "qwen-plus".into(),
                    backend: "dashscope".into(),
                    remote_model: "qwen3.5-plus".into(),
                    tier: None,
                    ctx: None,
                },
            ],
        );

        // plan phase
        let _ = router
            .stream(
                "deepseek-v4-flash",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .next()
            .await;
        // exec phase
        let _ = router
            .stream(
                "qwen-plus",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await
            .next()
            .await;

        assert_eq!(openrouter.seen.lock().unwrap()[0].0, "deepseek/deepseek-v4-flash");
        assert_eq!(openrouter.seen.lock().unwrap()[0].1.as_deref(), Some("or-key"));
        assert_eq!(dashscope.seen.lock().unwrap()[0].0, "qwen3.5-plus");
        assert_eq!(dashscope.seen.lock().unwrap()[0].1.as_deref(), Some("ds-key"));
    }
}

// StreamExt for .next() in tests
#[cfg(test)]
use futures_util::StreamExt;
