use pirs_rhai::{ExtensionHost, TrustDecision};

static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

#[test]
fn untrusted_project_dir_is_skipped() {
    let _g = ENV_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let proj = tmp.path().join("proj");
    std::fs::create_dir_all(proj.join(".pirs/extensions")).unwrap();
    std::fs::write(proj.join(".pirs/extensions/evil.rhai"), "register_tool(\"x\", \"x\", #{ type: \"object\" }); fn tool_x(args) { \"x\" }").unwrap();

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
    std::fs::write(ext.join("ok.rhai"), "register_tool(\"y\", \"y\", #{ type: \"object\" }); fn tool_y(args) { \"y\" }").unwrap();
    std::env::set_var("HOME", &home);

    let mut host = ExtensionHost::new();
    host.load_default_dirs_with_trust(tmp.path(), &mut |_| TrustDecision::Skip);
    let host = std::sync::Arc::new(host);
    assert_eq!(host.tools().len(), 1, "home extensions load regardless of project trust");
}
