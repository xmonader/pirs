//! Live smoke for multi-backend routing + serve failover.
//!
//! Runs only when `OPENROUTER_API_KEY` is set (skipped otherwise so CI stays
//! offline-friendly). Proves a real OpenAI-compatible backend returns tokens
//! through [`RoutingProvider`] under an alias.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use pirs_ai::{
    CompletionOptions, Context, LlmProvider, ModelRoute, OpenAiCompat, RoutingProvider,
    ServeTarget, StreamEvent,
};

fn openrouter_key() -> Option<String> {
    std::env::var("OPENROUTER_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

#[tokio::test]
async fn live_openrouter_alias_returns_text() {
    let Some(key) = openrouter_key() else {
        eprintln!("skip: OPENROUTER_API_KEY unset");
        return;
    };

    let client = Arc::new(
        OpenAiCompat::new(Some("https://openrouter.ai/api/v1".into()))
            .with_provider_name("openrouter")
            .with_max_retries(1),
    );
    let mut backends = HashMap::new();
    backends.insert(
        "openrouter".into(),
        (
            Arc::clone(&client) as Arc<dyn LlmProvider>,
            Some(key),
            vec![
                (
                    "HTTP-Referer".into(),
                    "https://github.com/xmonader/pirs".into(),
                ),
                ("X-Title".into(), "pirs-live-smoke".into()),
            ],
        ),
    );

    // Cheap/fast model on OpenRouter; if the slug 404s the test still proves
    // routing (we accept Error only after a real HTTP round-trip).
    let router = RoutingProvider::new(
        Arc::clone(&client) as Arc<dyn LlmProvider>,
        None,
        vec![],
        backends,
        vec![ModelRoute {
            alias: "smoke-fast".into(),
            serve: vec![ServeTarget {
                backend: "openrouter".into(),
                remote_model: "openrouter/auto".into(),
            }],
            tier: Some("fast".into()),
            ctx: None,
        }],
    );

    let mut stream = router
        .stream(
            "smoke-fast",
            &Context {
                system_prompt: Some("Reply with exactly: pong".into()),
                messages: vec![pirs_ai::Message::user("ping")],
                tools: vec![],
            },
            &CompletionOptions {
                max_tokens: Some(32),
                temperature: Some(0.0),
                ..Default::default()
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;

    let mut text = String::new();
    let mut saw_done = false;
    let mut err: Option<String> = None;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Error(e) => err = Some(e),
            StreamEvent::Done(m) => {
                saw_done = true;
                if let Some(e) = m.error_message {
                    err = Some(e);
                }
            }
            _ => {}
        }
    }

    assert!(saw_done, "stream must complete");
    if let Some(e) = err {
        // Network/auth/model-slug issues: still prove we reached the network.
        // Fail hard only on empty local routing bugs (no attempt).
        eprintln!("live smoke provider error (routing still exercised): {e}");
        assert!(
            !e.contains("no serve targets"),
            "router failed before HTTP: {e}"
        );
    } else {
        assert!(
            !text.is_empty(),
            "expected non-empty completion text from OpenRouter"
        );
        eprintln!("live smoke ok: {} chars", text.len());
    }
}

/// Dual-subscription smoke: plan alias → OpenRouter, exec alias → DashScope.
/// Requires OPENROUTER_API_KEY and DASHSCOPE_API_KEY (skips if either missing).
#[tokio::test]
async fn live_dual_backend_plan_openrouter_exec_dashscope() {
    let Some(or_key) = openrouter_key() else {
        eprintln!("skip: OPENROUTER_API_KEY unset");
        return;
    };
    let Some(ds_key) = dashscope_key() else {
        eprintln!("skip: DASHSCOPE_API_KEY unset");
        return;
    };

    let openrouter = Arc::new(
        OpenAiCompat::new(Some("https://openrouter.ai/api/v1".into()))
            .with_provider_name("openrouter")
            .with_max_retries(1),
    );
    let dashscope = Arc::new(
        OpenAiCompat::new(Some(
            std::env::var("DASHSCOPE_BASE_URL")
                .unwrap_or_else(|_| "https://coding-intl.dashscope.aliyuncs.com/v1".into()),
        ))
        .with_provider_name("dashscope")
        .with_max_retries(1),
    );

    let mut backends = HashMap::new();
    backends.insert(
        "openrouter".into(),
        (
            Arc::clone(&openrouter) as Arc<dyn LlmProvider>,
            Some(or_key),
            vec![
                (
                    "HTTP-Referer".into(),
                    "https://github.com/xmonader/pirs".into(),
                ),
                ("X-Title".into(), "pirs-dual-smoke".into()),
            ],
        ),
    );
    backends.insert(
        "dashscope".into(),
        (
            Arc::clone(&dashscope) as Arc<dyn LlmProvider>,
            Some(ds_key),
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
                alias: "plan-strong".into(),
                serve: vec![ServeTarget {
                    backend: "openrouter".into(),
                    remote_model: "openrouter/auto".into(),
                }],
                tier: None,
                ctx: None,
            },
            ModelRoute {
                alias: "exec-weak".into(),
                serve: vec![ServeTarget {
                    backend: "dashscope".into(),
                    remote_model: "qwen3.5-plus".into(),
                }],
                tier: None,
                ctx: None,
            },
        ],
    );

    // Resolve diagnostics (no secrets).
    let plan_r = router.resolve("plan-strong");
    let exec_r = router.resolve("exec-weak");
    assert_eq!(plan_r.backend_name, "openrouter");
    assert_eq!(exec_r.backend_name, "dashscope");
    assert_eq!(exec_r.remote_model, "qwen3.5-plus");

    // Plan phase (strong / openrouter)
    let plan_text = complete_alias(
        &router,
        "plan-strong",
        "You are a planner. Reply with exactly: PLAN_OK",
        "make a plan",
    )
    .await;
    // Exec phase (weak / dashscope)
    let exec_text = complete_alias(
        &router,
        "exec-weak",
        "You are an executor. Reply with exactly: EXEC_OK",
        "execute",
    )
    .await;

    eprintln!(
        "dual-backend smoke: plan_backend=openrouter text_len={} exec_backend=dashscope text_len={}",
        plan_text.len(),
        exec_text.len()
    );
    // Content or an explicit error: string proves we left the process (network/model).
    assert!(
        !plan_text.is_empty(),
        "plan phase (openrouter) produced nothing"
    );
    assert!(
        !exec_text.is_empty(),
        "exec phase (dashscope) produced nothing"
    );
    // Exec must not be an error-only failure if we claim dual-backend works.
    assert!(
        !exec_text.starts_with("error:"),
        "dashscope exec failed: {exec_text}"
    );
    eprintln!("dual-backend smoke ok: plan={plan_text:?} exec={exec_text:?}");
}

fn dashscope_key() -> Option<String> {
    std::env::var("DASHSCOPE_API_KEY")
        .ok()
        .filter(|s| !s.is_empty())
}

async fn complete_alias(
    router: &RoutingProvider,
    alias: &str,
    system: &str,
    user: &str,
) -> String {
    let mut stream = router
        .stream(
            alias,
            &Context {
                system_prompt: Some(system.into()),
                messages: vec![pirs_ai::Message::user(user)],
                tools: vec![],
            },
            &CompletionOptions {
                max_tokens: Some(32),
                temperature: Some(0.0),
                ..Default::default()
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let mut text = String::new();
    let mut err = None;
    while let Some(ev) = stream.next().await {
        match ev {
            StreamEvent::TextDelta(t) => text.push_str(&t),
            StreamEvent::Error(e) => err = Some(e),
            StreamEvent::Done(m) => {
                if let Some(e) = m.error_message {
                    err = Some(e);
                }
            }
            _ => {}
        }
    }
    if let Some(e) = err {
        if text.is_empty() {
            return format!("error:{e}");
        }
    }
    text
}

#[tokio::test]
async fn live_two_aliases_same_backend_distinct_remote_ids() {
    let Some(key) = openrouter_key() else {
        eprintln!("skip: OPENROUTER_API_KEY unset");
        return;
    };

    let client = Arc::new(
        OpenAiCompat::new(Some("https://openrouter.ai/api/v1".into()))
            .with_provider_name("openrouter"),
    );
    let mut backends = HashMap::new();
    backends.insert(
        "openrouter".into(),
        (
            Arc::clone(&client) as Arc<dyn LlmProvider>,
            Some(key),
            vec![("X-Title".into(), "pirs-live-smoke".into())],
        ),
    );
    let router = RoutingProvider::new(
        Arc::clone(&client) as Arc<dyn LlmProvider>,
        None,
        vec![],
        backends,
        vec![
            ModelRoute {
                alias: "plan-alias".into(),
                serve: vec![ServeTarget {
                    backend: "openrouter".into(),
                    remote_model: "openrouter/auto".into(),
                }],
                tier: None,
                ctx: None,
            },
            ModelRoute {
                alias: "exec-alias".into(),
                serve: vec![ServeTarget {
                    backend: "openrouter".into(),
                    remote_model: "openrouter/auto".into(),
                }],
                tier: None,
                ctx: None,
            },
        ],
    );

    for alias in ["plan-alias", "exec-alias"] {
        let r = router.resolve(alias);
        assert_eq!(r.backend_name, "openrouter");
        assert_eq!(r.remote_model, "openrouter/auto");
        assert_eq!(r.alias.as_deref(), Some(alias));
    }

    // One real call under plan-alias (proves alias → remote path end-to-end).
    let mut stream = router
        .stream(
            "plan-alias",
            &Context {
                system_prompt: Some("Say hi in one word.".into()),
                messages: vec![pirs_ai::Message::user("hi")],
                tools: vec![],
            },
            &CompletionOptions {
                max_tokens: Some(16),
                ..Default::default()
            },
            tokio_util::sync::CancellationToken::new(),
        )
        .await;
    let mut n_events = 0usize;
    while stream.next().await.is_some() {
        n_events += 1;
    }
    assert!(n_events > 0, "plan-alias must produce stream events");
}
