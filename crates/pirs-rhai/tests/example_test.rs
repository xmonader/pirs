#[test]
fn example_extension_loads_and_state_fns_work() {
    let src = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../extensions/word-count.rhai"
    ))
    .unwrap();
    let mut host = pirs_rhai::ExtensionHost::new();
    host.load_source(&src, "word-count.rhai".into()).unwrap();
    let host = std::sync::Arc::new(host);
    let hooks = host.hooks();
    let steering = hooks.get_steering_messages.unwrap();
    let msgs = steering();
    assert_eq!(msgs.len(), 1, "steering should inject once");
    assert!(steering().is_empty(), "second call must not inject again");
    let should_stop = hooks.should_stop_after_turn.unwrap();
    let ctx = pirs_ai::Context::default();
    assert!(!should_stop(&ctx));
}
