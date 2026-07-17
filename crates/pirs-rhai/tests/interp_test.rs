#[test]
fn rhai_interpolates_backtick_dollar_brace() {
    let engine = rhai::Engine::new();
    let v: String = engine.eval("let n = 3; `${n} words`").unwrap();
    assert_eq!(v, "3 words");
}
