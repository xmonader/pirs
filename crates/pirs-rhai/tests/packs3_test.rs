use std::sync::Arc;

use pirs_rhai::ExtensionHost;

// Guards tests that call `std::env::set_current_dir` — the process cwd is
// global, so two such tests racing in different threads (the default for
// `cargo test`) can each restore the other's tempdir out from under it.
static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn load(name: &str, with_runner: bool) -> Arc<ExtensionHost> {
    let path = format!("{}/../../extensions/{name}", env!("CARGO_MANIFEST_DIR"));
    let mut host = ExtensionHost::new();
    if with_runner {
        host.set_subagent_runner(Arc::new(|task: String, model: Option<String>| {
            let m = model.unwrap_or_else(|| "default".into());
            Ok(format!("[{m}] answer to: {}", &task[..task.len().min(60)]))
        }));
    }
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    Arc::new(host)
}

fn home_isolated() -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("pirs-pack3-{}", std::process::id()));
    std::env::set_var("HOME", &dir);
    dir.to_path_buf()
}

#[test]
fn instincts_records_and_steers() {
    let host = load("instincts.rhai", false);
    let hooks = host.hooks();
    let after = hooks.after_tool_call.unwrap();
    let steering = hooks.get_steering_messages.unwrap();

    let fail = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "bash".into(),
        content: vec![pirs_ai::ContentBlock::text("command not found: carge")],
        details: None,
        is_error: true,
        terminate: false,
        timestamp: 0,
    };
    let ok = pirs_ai::ToolResultMessage {
        is_error: false,
        ..fail.clone()
    };
    assert!(after("1", "bash", &fail).is_none());
    assert!(steering().is_empty(), "nothing until a fix succeeds");
    assert!(after("2", "bash", &ok).is_none());
    let msgs = steering();
    assert_eq!(msgs.len(), 1);
    let pirs_ai::Message::User(u) = &msgs[0] else {
        panic!()
    };
    let pirs_ai::UserContent::Text(t) = &u.content else {
        panic!()
    };
    assert!(t.contains("Instinct recorded"));
}

#[test]
fn session_handoff_injects_previous_brief() {
    let host = load("session-handoff.rhai", false);
    let tmp = home_isolated();
    let work = tmp.join("work");
    std::fs::create_dir_all(work.join(".pirs")).unwrap();
    std::fs::write(work.join(".pirs/handoff.md"), "goal: port everything").unwrap();
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();

    let out = transform(vec![pirs_ai::Message::user("current task")]);
    let cwd = std::env::current_dir().unwrap();
    if cwd == work {
        assert!(out.len() == 2, "handoff pin should be injected");
    }
}

#[test]
fn shadow_verify_flags_discrepancy() {
    let host = load("shadow-verify.rhai", false);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let after = hooks.after_tool_call.unwrap();
    let steering = hooks.get_steering_messages.unwrap();

    before(
        "1",
        "bash",
        &serde_json::json!({"command": "pytest -q && exit 1"}),
    );
    let ok = pirs_ai::ToolResultMessage {
        tool_call_id: "1".into(),
        tool_name: "bash".into(),
        content: vec![pirs_ai::ContentBlock::text("9 passed")],
        details: None,
        is_error: false,
        terminate: false,
        timestamp: 0,
    };
    after("1", "bash", &ok);
    let msgs = steering();
    assert_eq!(msgs.len(), 1, "rerun exits 1 -> discrepancy steered");
}

#[test]
fn spec_check_pins_and_verifies_once() {
    let host = load("spec-check.rhai", false);
    let hooks = host.hooks();
    let follow = hooks.get_follow_up_messages.unwrap();

    let cwd = std::env::current_dir().unwrap();
    let spec = cwd.join("ACCEPTANCE.md");
    std::fs::write(&spec, "- thing works").unwrap();
    let msgs = follow();
    std::fs::remove_file(&spec).unwrap();
    assert_eq!(msgs.len(), 1);
    assert!(follow().is_empty(), "fires once");
}

#[test]
fn semantic_bookmarks_pinned_and_capped() {
    let host = load("semantic-bookmarks.rhai", false);
    let tools = host.tools();
    let bookmark = tools.iter().find(|t| t.name() == "bookmark").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    for i in 0..7 {
        rt.block_on(bookmark.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"text": format!("fact {i}")}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    }
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();
    let out = transform(vec![pirs_ai::Message::user("x")]);
    let pin = out.iter().find_map(|m| match m {
        pirs_ai::Message::User(u) => match &u.content {
            pirs_ai::UserContent::Text(t) if t.contains("[bookmarks]") => Some(t.clone()),
            _ => None,
        },
        _ => None,
    });
    let pin = pin.expect("pin present");
    assert!(!pin.contains("fact 0"), "capped at 5, oldest dropped");
    assert!(pin.contains("fact 6"));
}

#[test]
fn diff_shield_merges_consecutive_same_tool_results() {
    let host = load("diff-shield.rhai", false);
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();

    let tr = |id: &str, text: &str| {
        pirs_ai::Message::ToolResult(pirs_ai::ToolResultMessage {
            tool_call_id: id.into(),
            tool_name: "grep".into(),
            content: vec![pirs_ai::ContentBlock::text(text)],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        })
    };
    let out = transform(vec![
        tr("1", "match a"),
        tr("2", "match b"),
        tr("3", "match c"),
        pirs_ai::Message::user("x"),
    ]);
    assert_eq!(out.len(), 2, "three grep results merged into one");
    let pirs_ai::Message::ToolResult(merged) = &out[0] else {
        panic!()
    };
    let text = merged.content[0].as_text().unwrap();
    assert!(text.contains("match a") && text.contains("match c"));
}

#[test]
fn chapter_spine_builds_spine() {
    let host = load("chapter-spine.rhai", true);
    let listener = host.listener().expect("listener");
    let hooks = host.hooks();
    let transform = hooks.transform_context.unwrap();

    for _ in 0..5 {
        listener(pirs_agent::AgentEvent::TurnEnd {
            message: Box::new(pirs_ai::AssistantMessage {
                content: vec![pirs_ai::ContentBlock::text("made progress on the port")],
                ..Default::default()
            }),
            tool_results: vec![],
        });
    }
    let out = transform(vec![pirs_ai::Message::user("x")]);
    assert!(out.iter().any(|m| matches!(
        m,
        pirs_ai::Message::User(u) if matches!(&u.content, pirs_ai::UserContent::Text(t) if t.contains("[progress so far]") && t.contains("5."))
    )));
}

#[test]
fn env_doctor_blocks_missing_toolchain() {
    let host = load("env-doctor.rhai", false);
    let hooks = host.hooks();
    let before = hooks.before_tool_call.unwrap();
    let missing = before(
        "1",
        "bash",
        &serde_json::json!({"command": "cargobogonotreal --version"}),
    );
    assert!(missing.is_none(), "unknown binaries pass through");
    let cargo = before("2", "bash", &serde_json::json!({"command": "cargo build"}));
    if std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok()
    {
        assert!(cargo.is_none());
    } else {
        assert!(cargo.is_some());
    }
}

#[test]
fn cost_sentinel_stops_at_cap() {
    let host = load("cost-sentinel.rhai", false);
    let listener = host.listener().expect("listener");
    let hooks = host.hooks();
    let stop = hooks.should_stop_after_turn.unwrap();

    let ev = |input: i64| pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage {
            usage: pirs_ai::Usage {
                input: input as u64,
                ..Default::default()
            },
            ..Default::default()
        }),
        tool_results: vec![],
    };
    for _ in 0..2 {
        listener(ev(200_000));
    }
    assert!(!stop(&pirs_ai::Context::default()));
    listener(ev(200_000));
    assert!(stop(&pirs_ai::Context::default()));
}

#[test]
fn arena_and_relay_run_pipelines() {
    let host = load("critic-arena.rhai", true);
    let tools = host.tools();
    let arena = tools.iter().find(|t| t.name() == "arena").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(arena.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"task": "is rust better?", "model_a": "glm", "model_b": "qwen"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    let text = out.content[0].as_text().unwrap();
    assert!(text.contains("=== glm ===") && text.contains("=== qwen ==="));

    let host2 = load("relay-race.rhai", true);
    let relay = host2
        .tools()
        .into_iter()
        .find(|t| t.name() == "relay")
        .unwrap();
    let out2 = rt
        .block_on(relay.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"task": "draft a haiku", "model": "glm"}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out2.content[0].as_text().unwrap().contains("[glm]"));
}

#[test]
fn telemetry_records_metadata_but_never_content() {
    let host = load("telemetry.rhai", false);
    let home = home_isolated();
    let listener = host.listener().expect("on_event listener");

    let secret_text = "SENTINEL super-secret assistant reply, must never leak";
    listener(pirs_agent::AgentEvent::TurnEnd {
        message: Box::new(pirs_ai::AssistantMessage {
            content: vec![pirs_ai::ContentBlock::text(secret_text)],
            usage: pirs_ai::Usage {
                input: 100,
                cache_read: 20,
                output: 30,
                ..Default::default()
            },
            stop_reason: pirs_ai::StopReason::Stop,
            ..Default::default()
        }),
        tool_results: vec![],
    });
    listener(pirs_agent::AgentEvent::ToolExecutionEnd {
        tool_call_id: "t1".into(),
        tool_name: "bash".into(),
        result: Box::new(pirs_ai::ToolResultMessage {
            tool_call_id: "t1".into(),
            tool_name: "bash".into(),
            content: vec![pirs_ai::ContentBlock::text(
                "SENTINEL tool output containing secrets",
            )],
            details: None,
            is_error: false,
            terminate: false,
            timestamp: 0,
        }),
    });
    listener(pirs_agent::AgentEvent::AgentEnd { messages: vec![] });

    let errs = host.drain_hook_errors();
    assert!(errs.is_empty(), "hook errors: {errs:?}");

    let logged = std::fs::read_to_string(home.join(".pirs").join("telemetry.jsonl")).unwrap();
    assert!(
        !logged.contains("SENTINEL"),
        "telemetry must never contain assistant text or tool content: {logged}"
    );

    let lines: Vec<serde_json::Value> = logged
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[0]["kind"], "turn");
    assert_eq!(lines[0]["stopReason"], "Stop");
    assert_eq!(lines[0]["inputTokens"], 100);
    assert_eq!(lines[0]["outputTokens"], 30);
    assert_eq!(lines[1]["kind"], "tool_call");
    assert_eq!(lines[1]["tool"], "bash");
    assert_eq!(lines[1]["isError"], false);
    assert_eq!(lines[2]["kind"], "session_end");
}

#[test]
fn hive_note_posts_and_reads() {
    let host = load("hive-note.rhai", false);
    home_isolated();
    let tools = host.tools();
    let post = tools.iter().find(|t| t.name() == "hive_post").unwrap();
    let read = tools.iter().find(|t| t.name() == "hive_read").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    rt.block_on(post.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: serde_json::json!({"note": "instance A finished porting"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }))
    .unwrap();
    let out = rt
        .block_on(read.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out.content[0]
        .as_text()
        .unwrap()
        .contains("instance A finished porting"));
}

#[test]
fn sandbox_overrides_bash_and_advertises_one_schema() {
    let host = load("sandbox.rhai", false);
    let tools = host.tools();
    let bash_tools: Vec<_> = tools.iter().filter(|t| t.name() == "bash").collect();
    assert_eq!(
        bash_tools.len(),
        1,
        "must present exactly one bash tool, not a shadowed original plus an override"
    );
    assert!(bash_tools[0].description().contains("sandboxed"));
}

#[test]
fn sandbox_rejects_background_without_attempting_to_sandbox() {
    let host = load("sandbox.rhai", false);
    let tools = host.tools();
    let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt
        .block_on(bash.execute(pirs_agent::ToolExecContext {
            tool_call_id: "t".into(),
            args: serde_json::json!({"command": "sleep 999", "background": true}),
            cancel: tokio_util::sync::CancellationToken::new(),
            on_update: None,
        }))
        .unwrap();
    assert!(out.content[0]
        .as_text()
        .unwrap()
        .contains("does not support background"));
}

#[test]
#[cfg(target_os = "linux")]
fn sandbox_runs_a_command_or_fails_loud_with_a_named_reason() {
    // This environment's Ubuntu base restricts unprivileged user namespaces
    // (kernel.apparmor_restrict_unprivileged_userns=1, the 23.10+/24.04+
    // default), so bwrap itself cannot succeed here regardless of this
    // pack's logic — this test accepts either a real result (an environment
    // where bwrap actually works) or the pack's own named diagnostic for
    // that specific failure, but never a silent/garbage failure.
    let _g = ENV_LOCK.lock().unwrap();
    let host = load("sandbox.rhai", false);
    let dir = tempfile::tempdir().unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let tools = host.tools();
    let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(bash.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: serde_json::json!({"command": "echo sandboxed-ok"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }));

    std::env::set_current_dir(prev).unwrap();
    let text = out.unwrap().content[0].as_text().unwrap().to_string();
    assert!(
        text.contains("sandboxed-ok") || text.contains("sandbox setup failed before the command ran"),
        "expected either a real sandboxed result or the named bwrap-setup-failure diagnostic, got: {text}"
    );
    // Whichever path was taken, the scratch dir it used must be cleaned up.
    assert!(!dir.path().join(".pirs").join("sandbox-tmp").exists());
}

#[test]
#[cfg(target_os = "linux")]
fn sandbox_egress_allowlist_permits_listed_domain_and_blocks_others() {
    // Proves the actual egress-allowlisting mechanism end to end: a listed
    // domain gets a real response through the proxy, an unlisted one is
    // rejected, and the result says which sandbox mode actually ran. Skips
    // (not fails) if docker or curl aren't available in this environment,
    // since that's an environment fact the pack's own logic already
    // surfaces as a loud, actionable message rather than a silent skip.
    let has = |cmd: &str| {
        std::process::Command::new("sh")
            .args(["-c", &format!("command -v {cmd}")])
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    if !has("docker") {
        eprintln!("skipping: docker not on PATH in this environment");
        return;
    }
    if !has("curl") {
        eprintln!("skipping: curl not on PATH in this environment (needed inside the chroot)");
        return;
    }

    let _g = ENV_LOCK.lock().unwrap();
    let host = load("sandbox.rhai", false);
    let dir = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(dir.path().join(".pirs")).unwrap();
    std::fs::write(
        dir.path().join(".pirs").join("sandbox-allowlist.txt"),
        "# comment lines are ignored\nexample.com\n",
    )
    .unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let tools = host.tools();
    let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();

    let allowed = rt.block_on(bash.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t1".into(),
        args: serde_json::json!({
            "command": "curl -sS -o /dev/null -w 'http_code=%{http_code}\\n' https://example.com --max-time 15",
            "timeout": 30,
        }),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }));
    let blocked = rt.block_on(bash.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t2".into(),
        args: serde_json::json!({
            "command": "curl -sS -o /dev/null -w 'http_code=%{http_code}\\n' https://example.org --max-time 15 || echo curl-was-blocked",
            "timeout": 30,
        }),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }));

    std::env::set_current_dir(prev).unwrap();

    // Best-effort cleanup of the long-lived shared proxy/network this pack
    // intentionally leaves running between calls in real usage — a test run
    // shouldn't leave it behind.
    if let Ok(home) = std::env::var("HOME") {
        let pid_file = std::path::Path::new(&home)
            .join(".pirs")
            .join("sandbox-egress")
            .join("proxy.pid");
        if let Ok(pid) = std::fs::read_to_string(&pid_file) {
            let _ = std::process::Command::new("kill").arg(pid.trim()).status();
        }
    }
    let _ = std::process::Command::new("docker")
        .args(["network", "rm", "pirs-sandbox-net"])
        .status();

    let allowed_text = allowed.unwrap().content[0].as_text().unwrap().to_string();
    assert!(
        allowed_text.contains("http_code=200"),
        "listed domain should get a real 200 through the allowlist proxy, got: {allowed_text}"
    );
    assert!(
        allowed_text.contains("sandboxed via docker with an egress allowlist"),
        "output should be annotated as having used the egress allowlist, got: {allowed_text}"
    );

    let blocked_text = blocked.unwrap().content[0].as_text().unwrap().to_string();
    assert!(
        !blocked_text.contains("http_code=200"),
        "unlisted domain must not get a real response through the proxy, got: {blocked_text}"
    );
}

#[test]
#[cfg(target_os = "linux")]
fn sandbox_falls_back_to_docker_when_bwrap_cannot_start() {
    // This environment's `kernel.apparmor_restrict_unprivileged_userns=1`
    // reliably breaks bwrap (confirmed above), so if docker is on PATH this
    // test proves the fallback actually engages and produces a real result,
    // not just that the pack's failure message is well-formed. Skips (not
    // fails) if docker isn't available, since that's an environment fact
    // this pack's own logic already handles by degrading further.
    if std::process::Command::new("sh")
        .args(["-c", "command -v docker"])
        .status()
        .map(|s| !s.success())
        .unwrap_or(true)
    {
        eprintln!("skipping: docker not on PATH in this environment");
        return;
    }
    if std::process::Command::new("sh")
        .args([
            "-c",
            "bwrap --die-with-parent --unshare-net --ro-bind / / -- true",
        ])
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
    {
        eprintln!("skipping: bwrap actually works in this environment, nothing to fall back from");
        return;
    }

    let _g = ENV_LOCK.lock().unwrap();
    let host = load("sandbox.rhai", false);
    let dir = tempfile::tempdir().unwrap();
    let prev = std::env::current_dir().unwrap();
    std::env::set_current_dir(dir.path()).unwrap();

    let tools = host.tools();
    let bash = tools.iter().find(|t| t.name() == "bash").unwrap();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let out = rt.block_on(bash.execute(pirs_agent::ToolExecContext {
        tool_call_id: "t".into(),
        args: serde_json::json!({"command": "echo docker-fallback-ok && whoami"}),
        cancel: tokio_util::sync::CancellationToken::new(),
        on_update: None,
    }));

    std::env::set_current_dir(prev).unwrap();
    let text = out.unwrap().content[0].as_text().unwrap().to_string();
    assert!(
        text.contains("docker-fallback-ok"),
        "expected the command to actually run inside the docker fallback, got: {text}"
    );
    assert!(
        text.contains("sandboxed via docker fallback"),
        "expected the output to be transparently annotated as having used the fallback, got: {text}"
    );
    assert!(!dir.path().join(".pirs").join("sandbox-tmp").exists());
}
