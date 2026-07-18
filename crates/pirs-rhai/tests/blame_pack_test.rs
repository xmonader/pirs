use std::sync::Arc;

use pirs_rhai::ExtensionHost;

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn git(repo: &std::path::Path, args: &[&str]) -> String {
    let out = std::process::Command::new("git")
        .args(args)
        .current_dir(repo)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_string()
}

/// blame.rhai annotates HEAD with (session, turn, model) when it moves
/// during a turn.
#[test]
fn blame_pack_notes_moved_head() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let repo = tmp.path();
    git(repo, &["init", "-q"]);
    git(repo, &["config", "user.email", "t@t"]);
    git(repo, &["config", "user.name", "t"]);
    std::fs::write(repo.join("f.txt"), "one\n").unwrap();
    git(repo, &["add", "f.txt"]);
    git(repo, &["commit", "-qm", "c1"]);

    let prev_cwd = std::env::current_dir().unwrap();
    std::env::set_current_dir(repo).unwrap();
    pirs_rhai::set_session_meta("sess-1", "qwen3-coder");

    let path = format!(
        "{}/../../examples/extensions/blame.rhai",
        env!("CARGO_MANIFEST_DIR")
    );
    let mut host = ExtensionHost::new();
    host.load_source(&std::fs::read_to_string(&path).unwrap(), path)
        .unwrap();
    let host = Arc::new(host);
    let listener = host.listener().expect("on_event listener");
    let turn_end = || {
        listener(pirs_agent::AgentEvent::TurnEnd {
            message: Box::new(pirs_ai::AssistantMessage::default()),
            tool_results: vec![],
        })
    };

    // Turn 1: HEAD is new to the pack -> note on the first commit.
    turn_end();
    let errs = host.drain_hook_errors();
    assert!(errs.is_empty(), "hook errors: {errs:?}");
    let head1 = git(repo, &["rev-parse", "HEAD"]);
    let note1 = git(repo, &["notes", "show", &head1]);
    assert!(
        note1.contains("pirs-session=sess-1")
            && note1.contains("pirs-turn=1")
            && note1.contains("pirs-model=qwen3-coder"),
        "{note1}"
    );

    // Turn 2 with no HEAD movement -> note unchanged (turn stays 1).
    turn_end();
    let note1b = git(repo, &["notes", "show", &head1]);
    assert!(note1b.contains("pirs-turn=1"), "{note1b}");

    // Agent commits -> next turn_end annotates the NEW head with turn 3.
    std::fs::write(repo.join("f.txt"), "one\ntwo\n").unwrap();
    git(repo, &["add", "f.txt"]);
    git(repo, &["commit", "-qm", "c2"]);
    turn_end();
    let head2 = git(repo, &["rev-parse", "HEAD"]);
    let note2 = git(repo, &["notes", "show", &head2]);
    assert!(note2.contains("pirs-turn=3"), "{note2}");

    pirs_rhai::set_session_meta("", "");
    std::env::set_current_dir(prev_cwd).unwrap();
}
