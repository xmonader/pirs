//! Delivery smoke: real `pirs-claw` binary entry points (no live LLM required).

use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn claw_bin() -> &'static str {
    env!("CARGO_BIN_EXE_pirs-claw")
}

fn run(args: &[&str]) -> (i32, String, String) {
    let out = Command::new(claw_bin())
        .args(args)
        .output()
        .expect("spawn pirs-claw");
    let code = out.status.code().unwrap_or(1);
    let stdout = String::from_utf8_lossy(&out.stdout).into_owned();
    let stderr = String::from_utf8_lossy(&out.stderr).into_owned();
    (code, stdout, stderr)
}

#[test]
fn help_lists_code_chat_schedule_serve() {
    let (code, stdout, stderr) = run(&["--help"]);
    let text = format!("{stdout}{stderr}");
    assert_eq!(code, 0, "help exit: {text}");
    for need in ["code", "chat", "history", "schedule", "serve"] {
        assert!(
            text.to_lowercase().contains(need),
            "help missing {need}:\n{text}"
        );
    }
}

#[test]
fn serve_telegram_fails_closed_without_token_or_allowlist() {
    // Without TELEGRAM_BOT_TOKEN and empty allowlist, gateway must not hang —
    // fail closed with a clear error.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    // Empty allowlist file still empty peers
    std::fs::write(state.join("allowlist.txt"), "# empty\n").unwrap();

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .env_remove("TELEGRAM_BOT_TOKEN")
        .env_remove("PIRS_TELEGRAM_BOT_TOKEN")
        .env_remove("PIRS_CLAW_ALLOW_ALL")
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "serve",
            "--channel",
            "telegram",
        ])
        .output()
        .expect("spawn serve");
    assert!(
        !out.status.success(),
        "serve without pairing/token must fail; out={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    assert!(
        text.contains("allowlist") || text.contains("telegram") || text.contains("token"),
        "expected pairing/token error:\n{text}"
    );
}

#[test]
fn serve_allow_all_warns_then_fails_without_token() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .env("PIRS_CLAW_ALLOW_ALL", "1")
        .env_remove("TELEGRAM_BOT_TOKEN")
        .env_remove("PIRS_TELEGRAM_BOT_TOKEN")
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "serve",
            "--channel",
            "telegram",
        ])
        .output()
        .expect("spawn serve");
    assert!(
        !out.status.success(),
        "must still fail without token; out={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(
        text.contains("PIRS_CLAW_ALLOW_ALL") || text.contains("pairing disabled"),
        "expected allow-all warning:\n{text}"
    );
    assert!(
        text.to_lowercase().contains("token"),
        "expected missing token after allow-all:\n{text}"
    );
}

#[test]
fn webhook_bind_host_is_localhost_in_source_default() {
    // Structural + unit-tested helper: the default must not be a world bind.
    //
    // Reads the whole `gateway/` module rather than one file. It used to
    // `include_str!("../src/gateway.rs")`, which stopped existing when 6d1adb3 split that file
    // into channel modules — and because `include_str!` of a missing path is a COMPILE error,
    // that broke `cargo test --workspace` for the entire repo, not just this test. The invariant
    // it guards (bind 127.0.0.1 unless explicitly opted in) survived the refactor untouched; only
    // the path did not.
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/gateway");
    let src: String = std::fs::read_dir(&dir)
        .expect("gateway module dir")
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().and_then(|x| x.to_str()) == Some("rs"))
        .map(|p| std::fs::read_to_string(p).unwrap_or_default())
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        !src.is_empty(),
        "gateway module is empty or unreadable at {}",
        dir.display()
    );
    assert!(
        src.contains("127.0.0.1"),
        "gateway must document/default localhost bind"
    );
    assert!(
        !src.contains("bind((\"0.0.0.0\"") && !src.contains("bind((\"0.0.0.0\","),
        "must not hardcode bind 0.0.0.0 without opt-in"
    );
}

#[test]
fn serve_rejects_unknown_channel() {
    let (code, stdout, stderr) = run(&["serve", "--channel", "irc"]);
    assert_ne!(code, 0);
    let text = format!("{stdout}{stderr}").to_lowercase();
    assert!(text.contains("unknown") || text.contains("supported"), "{text}");
}

#[test]
fn exec_rejects_modal() {
    let (code, stdout, stderr) = run(&["--exec", "modal", "chat", "x"]);
    assert_ne!(code, 0);
    let text = format!("{stdout}{stderr}").to_lowercase();
    assert!(text.contains("unsupported") || text.contains("modal"), "{text}");
}

#[test]
fn pair_add_list_remove_against_state_dir() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c1, o1, e1) = run(&["--state-dir", state, "pair", "add", "peer-aaa"]);
    assert_eq!(c1, 0, "add: {o1}{e1}");
    let (c2, o2, e2) = run(&["--state-dir", state, "pair", "add", "peer-bbb"]);
    assert_eq!(c2, 0, "add2: {o2}{e2}");
    let (c3, o3, e3) = run(&["--state-dir", state, "pair", "list"]);
    assert_eq!(c3, 0, "list: {o3}{e3}");
    assert!(o3.contains("peer-aaa"), "{o3}");
    assert!(o3.contains("peer-bbb"), "{o3}");
    let (c4, o4, e4) = run(&["--state-dir", state, "pair", "remove", "peer-aaa"]);
    assert_eq!(c4, 0, "remove: {o4}{e4}");
    let (c5, o5, e5) = run(&["--state-dir", state, "pair", "list"]);
    assert_eq!(c5, 0, "list2: {o5}{e5}");
    assert!(!o5.contains("peer-aaa"), "should not list removed peer: {o5}");
    assert!(o5.contains("peer-bbb"), "{o5}");
}

#[test]
fn session_path_scheme_in_source() {
    let src = include_str!("../src/session.rs");
    assert!(src.contains("sessions"), "multi-key sessions dir");
    assert!(src.contains("channel"), "channel key");
    assert!(src.contains("peer"), "peer key");
    assert!(src.contains("migrate_legacy") || src.contains("session.jsonl"));
}

#[test]
fn schedule_dry_run_tick_leaves_job_enabled() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c1, o1, e1) = run(&[
        "--state-dir",
        state,
        "schedule",
        "add",
        "--in",
        "0",
        "delivery-proof-job",
    ]);
    assert_eq!(c1, 0, "add: {o1}{e1}");
    assert!(o1.contains("scheduled") || o1.contains("job-"), "add out: {o1}");

    let (c2, o2, e2) = run(&["--state-dir", state, "schedule", "tick"]);
    assert_eq!(c2, 0, "tick dry-run: {o2}{e2}");
    assert!(
        o2.contains("due") || o2.contains("delivery-proof"),
        "tick should print due job: {o2}"
    );

    let (c3, o3, e3) = run(&["--state-dir", state, "schedule", "list"]);
    assert_eq!(c3, 0, "list: {o3}{e3}");
    assert!(
        o3.contains("enabled=true") || o3.contains("enabled= true"),
        "dry-run must leave job enabled:\n{o3}"
    );
    assert!(
        o3.contains("delivery-proof-job"),
        "job prompt still listed:\n{o3}"
    );
}

#[test]
fn chat_without_api_key_fails_closed_no_fake_reply() {
    // Strip common keys so require_llm_key fails even if host has secrets.env
    // loaded by the binary — we also use a throwaway HOME without secrets.
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("DASHSCOPE_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "chat",
            "should-not-get-fake-reply",
        ])
        .output()
        .expect("spawn chat");
    assert!(
        !out.status.success(),
        "chat without key must fail; stdout={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        !stdout.contains("(no reply)"),
        "must not invent fake assistant text: {stdout}"
    );
    let err = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        err.contains("api key") || err.contains("no api key"),
        "expected key error:\n{stderr}"
    );

    // Session must not contain a fake assistant line if user was never appended
    // after fail-before-session, or at most user-only if append happened first.
    // Our path fails before append when key missing — session file may be empty
    // or absent.
    let session = state.join("session.jsonl");
    if session.exists() {
        let body = std::fs::read_to_string(&session).unwrap_or_default();
        assert!(
            !body.contains("\"role\":\"assistant\"") && !body.contains("\"role\": \"assistant\""),
            "no assistant line on fail-closed:\n{body}"
        );
    }
}

#[test]
fn code_without_api_key_fails_closed() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    // Fake cargo project so auto-profile would prefer code if bare prompt used.
    let repo = dir.path().join("repo");
    std::fs::create_dir_all(&repo).unwrap();
    std::fs::write(repo.join("Cargo.toml"), "[package]\nname=\"t\"\nversion=\"0.1.0\"\n").unwrap();

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .env_remove("OPENAI_API_KEY")
        .env_remove("DASHSCOPE_API_KEY")
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENROUTER_API_KEY")
        .args(["-C", repo.to_str().unwrap(), "code", "noop-task"])
        .output()
        .expect("spawn code");
    assert!(
        !out.status.success(),
        "code without key must fail; out={}",
        String::from_utf8_lossy(&out.stdout)
    );
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    assert!(
        text.contains("api key") || text.contains("no api key"),
        "expected key error:\n{text}"
    );
}

#[test]
fn empty_invocation_exits_nonzero_with_usage() {
    let (code, stdout, stderr) = run(&[]);
    assert_ne!(code, 0);
    let text = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        text.contains("pirs-claw") || text.contains("usage") || text.contains("coding"),
        "expected usage hint:\n{text}"
    );
    let _ = SystemTime::now().duration_since(UNIX_EPOCH); // keep import used if optimized
}

#[test]
fn schedule_accepts_human_duration() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c1, o1, e1) = run(&[
        "--state-dir",
        state,
        "schedule",
        "add",
        "--in",
        "5m",
        "--every",
        "1h",
        "duration-job",
    ]);
    assert_eq!(c1, 0, "add duration: {o1}{e1}");
    assert!(o1.contains("scheduled") || o1.contains("job-"), "{o1}");
    assert!(
        o1.contains("every_secs=3600") || o1.contains("3600"),
        "1h → 3600s: {o1}"
    );
    let (c2, o2, e2) = run(&["--state-dir", state, "schedule", "list"]);
    assert_eq!(c2, 0, "list: {o2}{e2}");
    assert!(o2.contains("duration-job"), "{o2}");
}

#[test]
fn skills_list_and_add_show() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    let skills_src = dir.path().join("srcskill");
    std::fs::create_dir_all(&skills_src).unwrap();
    std::fs::write(
        skills_src.join("SKILL.md"),
        "---\nname: demo-skill\ndescription: demo\n---\n\n# Demo\nbody here\n",
    )
    .unwrap();
    std::fs::create_dir_all(home.join(".pirs")).unwrap();

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .args(["skills", "add", skills_src.to_str().unwrap()])
        .output()
        .expect("skills add");
    assert!(
        out.status.success(),
        "skills add: {}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .args(["skills", "list"])
        .output()
        .expect("skills list");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("demo-skill"), "list should show skill: {text}");

    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .args(["skills", "show", "demo-skill"])
        .output()
        .expect("skills show");
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    assert!(out.status.success(), "{text}");
    assert!(text.contains("body here") || text.contains("Demo"), "{text}");
}

#[test]
fn sessions_lists_empty_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c, o, e) = run(&["--state-dir", state, "sessions"]);
    assert_eq!(c, 0, "sessions: {o}{e}");
    assert!(
        o.contains("no sessions") || o.is_empty() || o.contains("cli"),
        "sessions out: {o}"
    );
}

#[test]
fn help_mentions_pair_and_skills() {
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0);
    let text = format!("{stdout}{stderr}").to_lowercase();
    assert!(text.contains("pair"), "help should list pair: {text}");
    assert!(text.contains("skills"), "help should list skills: {text}");
    assert!(text.contains("sessions"), "help should list sessions: {text}");
}

#[test]
fn schedule_list_shows_last_run_field() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c1, o1, e1) = run(&[
        "--state-dir",
        state,
        "schedule",
        "add",
        "--in",
        "99999",
        "--name",
        "later",
        "job-body",
    ]);
    assert_eq!(c1, 0, "{o1}{e1}");
    let (c2, o2, e2) = run(&["--state-dir", state, "schedule", "list"]);
    assert_eq!(c2, 0, "{o2}{e2}");
    assert!(
        o2.contains("last_run="),
        "schedule list must show last_run: {o2}"
    );
}

#[test]
fn help_mentions_no_extensions() {
    let (code, stdout, stderr) = run(&["--help"]);
    assert_eq!(code, 0);
    let text = format!("{stdout}{stderr}").to_lowercase();
    assert!(
        text.contains("no-extensions") || text.contains("extensions"),
        "help should mention extensions flag: {text}"
    );
}

#[test]
fn schedule_pause_and_remove() {
    let dir = tempfile::tempdir().unwrap();
    let state = dir.path().to_str().unwrap();
    let (c1, o1, e1) = run(&[
        "--state-dir",
        state,
        "schedule",
        "add",
        "--in",
        "0",
        "--name",
        "pulse",
        "do-pulse",
    ]);
    assert_eq!(c1, 0, "add: {o1}{e1}");
    let (c2, o2, e2) = run(&["--state-dir", state, "schedule", "pause", "pulse"]);
    assert_eq!(c2, 0, "pause: {o2}{e2}");
    let (c3, o3, e3) = run(&["--state-dir", state, "schedule", "list"]);
    assert_eq!(c3, 0, "list: {o3}{e3}");
    assert!(o3.contains("enabled=false"), "paused job: {o3}");
    let (c4, o4, e4) = run(&["--state-dir", state, "schedule", "remove", "pulse"]);
    assert_eq!(c4, 0, "remove: {o4}{e4}");
    let (c5, o5, e5) = run(&["--state-dir", state, "schedule", "list"]);
    assert_eq!(c5, 0, "{o5}{e5}");
    assert!(!o5.contains("do-pulse"), "removed: {o5}");
}

#[test]
fn skills_validate_rejects_bad_name() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    let bad = dir.path().join("BadName.md");
    std::fs::write(
        &bad,
        "---\nname: BadName\ndescription: x\n---\nbody\n",
    )
    .unwrap();
    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .args(["skills", "validate", bad.to_str().unwrap()])
        .output()
        .expect("validate");
    assert!(
        !out.status.success(),
        "bad agentskills name must fail: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

#[test]
fn serve_all_fails_without_credentials() {
    let dir = tempfile::tempdir().unwrap();
    let home = dir.path().join("home");
    std::fs::create_dir_all(home.join(".pirs")).unwrap();
    let state = dir.path().join("state");
    std::fs::create_dir_all(&state).unwrap();
    std::fs::write(state.join("allowlist.txt"), "peer1\n").unwrap();
    let out = Command::new(claw_bin())
        .env("HOME", &home)
        .env("PIRS_CLAW_ALLOW_ALL", "0")
        .env_remove("TELEGRAM_BOT_TOKEN")
        .env_remove("PIRS_TELEGRAM_BOT_TOKEN")
        .env_remove("DISCORD_BOT_TOKEN")
        .env_remove("SLACK_BOT_TOKEN")
        .env_remove("WHATSAPP_TOKEN")
        .env_remove("PIRS_CLAW_ALLOW_ALL")
        .args([
            "--state-dir",
            state.to_str().unwrap(),
            "serve",
            "--channel",
            "all",
        ])
        .output()
        .expect("serve all");
    assert!(!out.status.success());
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
    .to_lowercase();
    assert!(
        text.contains("no gateway") || text.contains("token") || text.contains("telegram"),
        "expected multi-channel fail: {text}"
    );
}
