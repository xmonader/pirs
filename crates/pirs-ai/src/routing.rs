//! Multi-backend model routing with ordered serve failover.
//!
//! A [`RoutingProvider`] maps **aliases** (what the user types as `--model` /
//! `--plan-model`) onto an ordered list of serve targets (backend + remote
//! model id). On stream failure (HTTP/error stop before any content), the next
//! serve entry is tried. Unregistered model names fall through to a default
//! provider unchanged.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;

use crate::{
    AssistantMessage, CompletionOptions, ContentBlock, Context, LlmProvider, StopReason,
    StreamEvent,
};

/// One authenticated API endpoint.
#[derive(Debug, Clone)]
pub struct BackendSpec {
    pub name: String,
    pub kind: BackendKind,
    pub base_url: String,
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

/// One concrete serve target: backend name + remote model id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeTarget {
    pub backend: String,
    pub remote_model: String,
}

/// Alias → ordered serve list (first is primary; rest are failover).
#[derive(Debug, Clone)]
pub struct ModelRoute {
    pub alias: String,
    pub serve: Vec<ServeTarget>,
    pub tier: Option<String>,
    pub ctx: Option<u64>,
}

/// Built routing table + live providers per backend.
pub struct RoutingProvider {
    routes: HashMap<String, ModelRoute>,
    backends: HashMap<String, BackendHandle>,
    default: BackendHandle,
}

struct BackendHandle {
    provider: Arc<dyn LlmProvider>,
    api_key: Option<String>,
    headers: Vec<(String, String)>,
    name: String,
}

/// Provider + optional key + static headers for one named backend (constructor input).
type BackendParts = (Arc<dyn LlmProvider>, Option<String>, Vec<(String, String)>);

impl RoutingProvider {
    pub fn new(
        default: Arc<dyn LlmProvider>,
        default_api_key: Option<String>,
        default_headers: Vec<(String, String)>,
        backends: HashMap<String, BackendParts>,
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
        let route_map = routes.into_iter().map(|r| (r.alias.clone(), r)).collect();
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

    /// Ordered serve targets for a model spec.
    ///
    /// 1. **Pin** `backend/remote` (first `/`) → single target if backend exists
    ///    and has a key; otherwise empty (caller / stream reports error).
    /// 2. **Portable** bare name → alias route, only backends with keys.
    /// 3. Legacy fallback: raw id on the default provider (single-backend setups).
    pub fn targets_for(&self, model_or_alias: &str) -> Vec<ResolvedRef> {
        use crate::model_ref::ModelSpec;

        match ModelSpec::parse(model_or_alias) {
            ModelSpec::Pin { backend, remote } => {
                if let Some(handle) = self.backends.get(&backend) {
                    // Require key when the handle was built with one expected —
                    // empty api_key means unset env; still allow for keyless backends.
                    if handle.api_key.is_none()
                        && self
                            .backends
                            .get(&backend)
                            .is_some_and(|h| h.name == backend)
                    {
                        // Allow pin even without key so the provider can error clearly;
                        // prefer skipping only when we know the env was required.
                        // Heuristic: if *any* other backend has a key and this doesn't,
                        // still return the pin so the user sees auth failure on that sub.
                    }
                    return vec![ResolvedRef {
                        alias: None,
                        backend_name: handle.name.clone(),
                        remote_model: remote,
                        provider: Arc::clone(&handle.provider),
                        api_key: handle.api_key.clone(),
                        headers: handle.headers.clone(),
                    }];
                }
                // Unknown pin backend → empty (stream emits error).
                return Vec::new();
            }
            ModelSpec::Portable(name) => {
                if let Some(route) = self.routes.get(&name) {
                    let mut out = Vec::new();
                    for t in &route.serve {
                        if let Some(backend) = self.backends.get(&t.backend) {
                            // Skip backends with no key when other targets exist.
                            if backend.api_key.is_none() {
                                continue;
                            }
                            out.push(ResolvedRef {
                                alias: Some(route.alias.clone()),
                                backend_name: backend.name.clone(),
                                remote_model: t.remote_model.clone(),
                                provider: Arc::clone(&backend.provider),
                                api_key: backend.api_key.clone(),
                                headers: backend.headers.clone(),
                            });
                        }
                    }
                    // If all keys missing, still try entries without filtering so
                    // the user gets a provider auth error rather than silence.
                    if out.is_empty() {
                        for t in &route.serve {
                            if let Some(backend) = self.backends.get(&t.backend) {
                                out.push(ResolvedRef {
                                    alias: Some(route.alias.clone()),
                                    backend_name: backend.name.clone(),
                                    remote_model: t.remote_model.clone(),
                                    provider: Arc::clone(&backend.provider),
                                    api_key: backend.api_key.clone(),
                                    headers: backend.headers.clone(),
                                });
                            }
                        }
                    }
                    if !out.is_empty() {
                        return out;
                    }
                }
                // Bare name, no portable entry: default provider (legacy).
                vec![ResolvedRef {
                    alias: None,
                    backend_name: self.default.name.clone(),
                    remote_model: name,
                    provider: Arc::clone(&self.default.provider),
                    api_key: self.default.api_key.clone(),
                    headers: self.default.headers.clone(),
                }]
            }
        }
    }

    /// First resolve (primary serve) — for diagnostics.
    pub fn resolve(&self, model_or_alias: &str) -> ResolvedRef {
        self.targets_for(model_or_alias)
            .into_iter()
            .next()
            .unwrap_or_else(|| ResolvedRef {
                alias: None,
                backend_name: self.default.name.clone(),
                remote_model: model_or_alias.to_string(),
                provider: Arc::clone(&self.default.provider),
                api_key: self.default.api_key.clone(),
                headers: self.default.headers.clone(),
            })
    }

    pub fn has_alias(&self, name: &str) -> bool {
        self.routes.contains_key(name)
    }

    pub fn has_backend(&self, name: &str) -> bool {
        self.backends.contains_key(name)
    }

    pub fn backend_names(&self) -> Vec<&str> {
        let mut v: Vec<_> = self.backends.keys().map(|s| s.as_str()).collect();
        v.sort_unstable();
        v
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

fn options_for(resolved: &ResolvedRef, options: &CompletionOptions) -> CompletionOptions {
    let mut opts = options.clone();
    if let Some(key) = resolved.api_key.clone() {
        opts.api_key = Some(key);
    }
    let mut headers = resolved.headers.clone();
    headers.extend(opts.extra_headers.iter().cloned());
    opts.extra_headers = headers;
    opts
}

/// Classify early stream events: hard fail → try next serve; content → commit.
enum Probe {
    Fail(String),
    Commit(Vec<StreamEvent>),
}

async fn probe_stream(
    mut stream: futures_util::stream::BoxStream<'static, StreamEvent>,
) -> (Probe, futures_util::stream::BoxStream<'static, StreamEvent>) {
    let mut buffered = Vec::new();
    loop {
        match stream.next().await {
            None => {
                if buffered.is_empty() {
                    return (Probe::Fail("empty stream".into()), stream);
                }
                return (Probe::Commit(buffered), stream);
            }
            Some(StreamEvent::Error(e)) => {
                return (Probe::Fail(e), stream);
            }
            Some(StreamEvent::Done(m))
                if m.stop_reason == StopReason::Error
                    || m.error_message.as_ref().is_some_and(|e| !e.is_empty()) =>
            {
                let msg = m
                    .error_message
                    .clone()
                    .unwrap_or_else(|| "provider error".into());
                return (Probe::Fail(msg), stream);
            }
            Some(ev @ StreamEvent::TextDelta(_))
            | Some(ev @ StreamEvent::ThinkingDelta(_))
            | Some(ev @ StreamEvent::ToolCallDelta)
            | Some(ev @ StreamEvent::Done(_)) => {
                buffered.push(ev);
                return (Probe::Commit(buffered), stream);
            }
            Some(ev @ StreamEvent::Start) => {
                buffered.push(ev);
                // keep reading until content or error
            }
        }
    }
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
        let targets = self.targets_for(model);
        let mut last_err = String::from("no serve targets");
        let n = targets.len();
        if n == 0 {
            let msg = format!(
                "no serve targets for {model:?} — use backend/model (e.g. dashscope/qwen3.5-plus) \
                 or a portable name from `pirs models`; check `pirs backends` for keys"
            );
            return Box::pin(futures_util::stream::iter(vec![
                StreamEvent::Error(msg.clone()),
                StreamEvent::Done(Box::new(AssistantMessage {
                    content: vec![ContentBlock::text(msg.clone())],
                    stop_reason: StopReason::Error,
                    error_message: Some(msg),
                    ..Default::default()
                })),
            ]));
        }

        for (i, resolved) in targets.into_iter().enumerate() {
            if cancel.is_cancelled() {
                return Box::pin(futures_util::stream::iter(vec![StreamEvent::Done(
                    Box::new(AssistantMessage {
                        content: vec![],
                        stop_reason: StopReason::Aborted,
                        ..Default::default()
                    }),
                )]));
            }
            let opts = options_for(&resolved, options);
            let stream = resolved
                .provider
                .stream(&resolved.remote_model, context, &opts, cancel.clone())
                .await;
            let (probe, rest) = probe_stream(stream).await;
            match probe {
                Probe::Fail(e) => {
                    last_err = format!(
                        "backend {} model {}: {e}",
                        resolved.backend_name, resolved.remote_model
                    );
                    if i + 1 < n {
                        tracing::warn!(
                            alias = model,
                            backend = %resolved.backend_name,
                            remote = %resolved.remote_model,
                            error = %e,
                            "serve target failed; trying next"
                        );
                        eprintln!(
                            "[model registry: {} via {} failed ({e}); trying next serve target]",
                            resolved.remote_model, resolved.backend_name
                        );
                    }
                    continue;
                }
                Probe::Commit(buffered) => {
                    return Box::pin(futures_util::stream::iter(buffered).chain(rest));
                }
            }
        }

        // All serve targets failed.
        Box::pin(futures_util::stream::iter(vec![
            StreamEvent::Error(last_err.clone()),
            StreamEvent::Done(Box::new(AssistantMessage {
                content: vec![ContentBlock::text(format!("all serve targets failed: {last_err}"))],
                stop_reason: StopReason::Error,
                error_message: Some(last_err),
                ..Default::default()
            })),
        ]))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    type CaptureCall = (String, Option<String>, Vec<(String, String)>);

    struct CaptureProvider {
        seen: Mutex<Vec<CaptureCall>>,
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
            Box::pin(futures_util::stream::iter(vec![
                StreamEvent::Start,
                StreamEvent::TextDelta(format!("{}:{}", self.label, model)),
                StreamEvent::Done(Box::new(msg)),
            ]))
        }
    }

    /// Fails the first `fail_count` calls with Error Done, then succeeds.
    struct FailThenOk {
        calls: AtomicUsize,
        fail_count: usize,
        label: String,
    }

    #[async_trait::async_trait]
    impl LlmProvider for FailThenOk {
        async fn stream(
            &self,
            model: &str,
            _context: &Context,
            _options: &CompletionOptions,
            _cancel: tokio_util::sync::CancellationToken,
        ) -> futures_util::stream::BoxStream<'static, StreamEvent> {
            let n = self.calls.fetch_add(1, Ordering::SeqCst);
            if n < self.fail_count {
                let msg = AssistantMessage {
                    content: vec![],
                    stop_reason: StopReason::Error,
                    error_message: Some(format!("fail-{n}")),
                    ..Default::default()
                };
                return Box::pin(futures_util::stream::iter(vec![StreamEvent::Done(
                    Box::new(msg),
                )]));
            }
            let text = format!("{}:{}", self.label, model);
            let msg = AssistantMessage {
                content: vec![ContentBlock::text(&text)],
                stop_reason: StopReason::Stop,
                ..Default::default()
            };
            Box::pin(futures_util::stream::iter(vec![
                StreamEvent::Start,
                StreamEvent::TextDelta(text.clone()),
                StreamEvent::Done(Box::new(msg)),
            ]))
        }
    }

    fn route(alias: &str, serve: Vec<(&str, &str)>) -> ModelRoute {
        ModelRoute {
            alias: alias.into(),
            serve: serve
                .into_iter()
                .map(|(b, m)| ServeTarget {
                    backend: b.into(),
                    remote_model: m.into(),
                })
                .collect(),
            tier: None,
            ctx: None,
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
                route(
                    "deepseek-v4-flash",
                    vec![("openrouter", "deepseek/deepseek-v4-flash")],
                ),
                route("qwen-plus", vec![("dashscope", "qwen3.5-plus")]),
            ],
        );

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
                route(
                    "deepseek-v4-flash",
                    vec![("openrouter", "deepseek/deepseek-v4-flash")],
                ),
                route("qwen-plus", vec![("dashscope", "qwen3.5-plus")]),
            ],
        );

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

        assert_eq!(
            openrouter.seen.lock().unwrap()[0].0,
            "deepseek/deepseek-v4-flash"
        );
        assert_eq!(
            openrouter.seen.lock().unwrap()[0].1.as_deref(),
            Some("or-key")
        );
        assert_eq!(dashscope.seen.lock().unwrap()[0].0, "qwen3.5-plus");
        assert_eq!(
            dashscope.seen.lock().unwrap()[0].1.as_deref(),
            Some("ds-key")
        );
    }

    #[tokio::test]
    async fn failover_tries_second_serve_when_first_errors() {
        let primary = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 100, // always fail
            label: "primary".into(),
        });
        let secondary = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 0, // always ok
            label: "secondary".into(),
        });
        let mut backends = HashMap::new();
        backends.insert(
            "a".into(),
            (Arc::clone(&primary) as Arc<dyn LlmProvider>, None, vec![]),
        );
        backends.insert(
            "b".into(),
            (
                Arc::clone(&secondary) as Arc<dyn LlmProvider>,
                Some("b-key".into()),
                vec![],
            ),
        );
        let router = RoutingProvider::new(
            Arc::clone(&primary) as Arc<dyn LlmProvider>,
            None,
            vec![],
            backends,
            vec![route(
                "flash",
                vec![("a", "model-a"), ("b", "model-b")],
            )],
        );

        let mut stream = router
            .stream(
                "flash",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        let mut texts = Vec::new();
        while let Some(ev) = stream.next().await {
            if let StreamEvent::TextDelta(t) = ev {
                texts.push(t);
            }
        }
        assert!(
            texts.iter().any(|t| t.contains("secondary:model-b")),
            "failover must reach secondary: {texts:?}"
        );
        // Backend "a" has no API key → skipped for portable routes; only "b" runs.
        assert_eq!(primary.calls.load(Ordering::SeqCst), 0);
        assert_eq!(secondary.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn portable_failover_when_first_keyed_backend_errors() {
        let primary = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 100,
            label: "primary".into(),
        });
        let secondary = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 0,
            label: "secondary".into(),
        });
        let mut backends = HashMap::new();
        backends.insert(
            "a".into(),
            (
                Arc::clone(&primary) as Arc<dyn LlmProvider>,
                Some("a-key".into()),
                vec![],
            ),
        );
        backends.insert(
            "b".into(),
            (
                Arc::clone(&secondary) as Arc<dyn LlmProvider>,
                Some("b-key".into()),
                vec![],
            ),
        );
        let router = RoutingProvider::new(
            Arc::clone(&primary) as Arc<dyn LlmProvider>,
            None,
            vec![],
            backends,
            vec![route("flash", vec![("a", "model-a"), ("b", "model-b")])],
        );
        let mut stream = router
            .stream(
                "flash",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        let mut texts = Vec::new();
        while let Some(ev) = stream.next().await {
            if let StreamEvent::TextDelta(t) = ev {
                texts.push(t);
            }
        }
        assert!(
            texts.iter().any(|t| t.contains("secondary:model-b")),
            "{texts:?}"
        );
        assert_eq!(primary.calls.load(Ordering::SeqCst), 1);
        assert_eq!(secondary.calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn pin_targets_specific_backend() {
        let a = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 0,
            label: "or".into(),
        });
        let b = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 0,
            label: "ds".into(),
        });
        let mut backends = HashMap::new();
        backends.insert(
            "openrouter".into(),
            (a as Arc<dyn LlmProvider>, Some("k1".into()), vec![]),
        );
        backends.insert(
            "dashscope".into(),
            (b as Arc<dyn LlmProvider>, Some("k2".into()), vec![]),
        );
        let router = RoutingProvider::new(
            Arc::new(FailThenOk {
                calls: AtomicUsize::new(0),
                fail_count: 0,
                label: "def".into(),
            }) as Arc<dyn LlmProvider>,
            None,
            vec![],
            backends,
            vec![],
        );
        let t = router.targets_for("openrouter/deepseek/deepseek-v4-flash");
        assert_eq!(t.len(), 1);
        assert_eq!(t[0].backend_name, "openrouter");
        assert_eq!(t[0].remote_model, "deepseek/deepseek-v4-flash");
    }

    #[tokio::test]
    async fn all_serve_targets_failing_surfaces_error() {
        let primary = Arc::new(FailThenOk {
            calls: AtomicUsize::new(0),
            fail_count: 100,
            label: "a".into(),
        });
        let mut backends = HashMap::new();
        backends.insert(
            "a".into(),
            (Arc::clone(&primary) as Arc<dyn LlmProvider>, None, vec![]),
        );
        let router = RoutingProvider::new(
            Arc::clone(&primary) as Arc<dyn LlmProvider>,
            None,
            vec![],
            backends,
            vec![route("x", vec![("a", "m1"), ("a", "m2")])],
        );
        let mut stream = router
            .stream(
                "x",
                &Context::default(),
                &CompletionOptions::default(),
                tokio_util::sync::CancellationToken::new(),
            )
            .await;
        let mut saw_error = false;
        while let Some(ev) = stream.next().await {
            if matches!(ev, StreamEvent::Error(_)) {
                saw_error = true;
            }
            if let StreamEvent::Done(m) = ev {
                assert_eq!(m.stop_reason, StopReason::Error);
            }
        }
        assert!(saw_error);
        assert_eq!(primary.calls.load(Ordering::SeqCst), 2);
    }
}
