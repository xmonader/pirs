use pirs_graph::{Graph, SymKind};

fn fixture() -> (tempfile::TempDir, Graph) {
    let dir = tempfile::tempdir().unwrap();
    std::fs::write(
        dir.path().join("main.rs"),
        r#"
fn main() {
    helper();
    parse_config();
}

fn helper() {
    parse_config();
}

fn parse_config() -> u32 {
    42
}

struct Config {
    value: u32,
}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("app.py"),
        r#"
def boot():
    setup()

def setup():
    pass

class Server:
    pass
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("web.ts"),
        r#"
function render(): void {
    mount();
}

function mount(): void {}

class App {}
"#,
    )
    .unwrap();
    std::fs::write(
        dir.path().join("main.go"),
        r#"
package main

func run() {
    start()
}

func start() {}
"#,
    )
    .unwrap();
    let graph = Graph::build(dir.path());
    (dir, graph)
}

#[test]
fn finds_definitions_across_languages() {
    let (_dir, g) = fixture();
    for name in [
        "main",
        "helper",
        "parse_config",
        "boot",
        "setup",
        "render",
        "mount",
        "run",
        "start",
    ] {
        assert!(!g.symbol(name).is_empty(), "missing: {name}");
    }
    assert_eq!(g.symbol("parse_config")[0].kind, SymKind::Function);
    assert_eq!(g.symbol("Config")[0].kind, SymKind::Struct);
    assert_eq!(g.symbol("Server")[0].kind, SymKind::Class);
    assert_eq!(g.symbol("App")[0].kind, SymKind::Class);
}

#[test]
fn callers_and_callees() {
    let (_dir, g) = fixture();
    let callers = g.callers("parse_config");
    let names: Vec<&str> = callers.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"main"));
    assert!(names.contains(&"helper"));

    let callees = g.callees("main");
    assert!(callees.contains(&"helper".to_string()));
    assert!(callees.contains(&"parse_config".to_string()));
}

#[test]
fn pagerank_ranks_callee_higher() {
    let (_dir, g) = fixture();
    let top = g.top(3);
    assert_eq!(
        top[0].0.name, "parse_config",
        "most-called symbol ranks first: {top:?}"
    );
}

#[test]
fn file_symbols_map() {
    let (dir, g) = fixture();
    let syms = g.file_symbols(&dir.path().join("app.py"));
    let names: Vec<&str> = syms.iter().map(|s| s.name.as_str()).collect();
    assert!(names.contains(&"boot"));
    assert!(names.contains(&"Server"));
}

#[test]
fn affected_tests_direct_callers_and_same_file() {
    let dir = tempfile::tempdir().unwrap();
    let src = dir.path().join("src");
    std::fs::create_dir_all(src.join("tests")).unwrap();
    std::fs::write(
        src.join("lib.rs"),
        r#"
fn parse_config() -> u32 { 42 }
fn helper() { parse_config(); }
"#,
    )
    .unwrap();
    std::fs::write(
        src.join("tests").join("config_test.rs"),
        r#"
fn test_parse_config() { parse_config(); }
fn unrelated_helper() {}
"#,
    )
    .unwrap();
    let graph = Graph::build(dir.path());

    let hit = graph.affected_tests(&src.join("lib.rs"));
    assert!(hit.iter().any(|t| t == "test_parse_config"), "{hit:?}");
    assert!(!hit.iter().any(|t| t == "unrelated_helper"), "{hit:?}");

    // Editing the test file itself marks its own tests affected.
    let own = graph.affected_tests(&src.join("tests").join("config_test.rs"));
    assert!(own.iter().any(|t| t == "test_parse_config"), "{own:?}");

    // A file with no graph symbols yields nothing, not a panic.
    assert!(graph.affected_tests(&src.join("nope.rs")).is_empty());
}
