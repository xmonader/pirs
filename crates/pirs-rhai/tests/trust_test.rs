use pirs_rhai::{ExtensionHost, TrustDecision};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn untrusted_project_dir_is_skipped() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join(".pirs/extensions")).unwrap();
    std::fs::write(
        proj.join(".pirs/extensions/evil.rhai"),
        "register_tool(\"x\", \"x\", #{ type: \"object\" }); fn tool_x(args) { \"x\" }",
    )
    .unwrap();

    let mut host = ExtensionHost::new();
    host.load_default_dirs_with_trust(&proj, &mut |_| TrustDecision::Deny);
    let host = std::sync::Arc::new(host);
    assert!(host.tools().is_empty(), "denied dir must not load tools");
    assert!(host.load_errors.iter().any(|e| e.contains("untrusted")));

    let mut host2 = ExtensionHost::new();
    host2.load_default_dirs_with_trust(&proj, &mut |_| TrustDecision::Allow);
    let host2 = std::sync::Arc::new(host2);
    assert_eq!(host2.tools().len(), 1);
}

#[test]
fn home_dir_extensions_always_trusted() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let home = tmp.path().join("home");
    let ext = home.join(".pirs/extensions");
    std::fs::create_dir_all(&ext).unwrap();
    std::fs::write(
        ext.join("ok.rhai"),
        "register_tool(\"y\", \"y\", #{ type: \"object\" }); fn tool_y(args) { \"y\" }",
    )
    .unwrap();
    std::env::set_var("HOME", &home);

    let mut host = ExtensionHost::new();
    host.load_default_dirs_with_trust(tmp.path(), &mut |_| TrustDecision::Skip);
    let host = std::sync::Arc::new(host);
    assert_eq!(
        host.tools().len(),
        1,
        "home extensions load regardless of project trust"
    );
}

#[test]
fn trust_directory_roundtrips_through_load() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    // Isolate HOME: trusted.json lives there and the implicit home-extension
    // trust must not misfire on the real home dir.
    let home = tmp.path().join("home");
    std::fs::create_dir_all(&home).unwrap();
    let prev_home = std::env::var("HOME").ok();
    std::env::set_var("HOME", &home);

    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join(".pirs/extensions")).unwrap();
    let script = proj.join(".pirs/extensions/tool.rhai");
    std::fs::write(
        &script,
        "register_tool(\"x\", \"x\", #{ type: \"object\" }); fn tool_x(args) { \"x\" }",
    )
    .unwrap();

    // Non-terminal stdin: untrusted project dirs are denied, not prompted.
    let mut h1 = ExtensionHost::new();
    h1.load_default_dirs(&proj);
    let h1 = std::sync::Arc::new(h1);
    assert!(h1.tools().is_empty(), "untrusted dir must not load");

    pirs_rhai::trust_directory(&proj).expect("trust_directory");
    let mut h2 = ExtensionHost::new();
    h2.load_default_dirs(&proj);
    let h2 = std::sync::Arc::new(h2);
    assert_eq!(h2.tools().len(), 1, "trusted dir must load");

    // Trust is content-hash-pinned: modifying a script invalidates it
    // (git pull re-prompts instead of silently running new code).
    std::fs::write(&script, "// changed\nfn tool_x(args) { \"y\" }").unwrap();
    let mut h3 = ExtensionHost::new();
    h3.load_default_dirs(&proj);
    let h3 = std::sync::Arc::new(h3);
    assert!(
        h3.tools().is_empty(),
        "modified content must invalidate trust"
    );

    match prev_home {
        Some(h) => std::env::set_var("HOME", h),
        None => std::env::remove_var("HOME"),
    }
}
