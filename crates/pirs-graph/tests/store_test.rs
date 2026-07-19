//! The persistent store's contract: an incrementally-refreshed graph must be
//! set-equivalent to a from-scratch parse, and refreshes must re-parse only what
//! changed. If either breaks, the persistent path is unsafe to trust.

use std::fs;
use std::path::Path;

use pirs_graph::store::{full_graph, GraphStore};
use pirs_graph::{Graph, Symbol};

/// A stable, order-independent fingerprint of a graph's symbol set.
fn fingerprint(g: &Graph) -> Vec<String> {
    let mut v: Vec<String> = g
        .symbols
        .iter()
        .map(|s: &Symbol| {
            let mut calls = s.calls.clone();
            calls.sort();
            format!(
                "{}|{}|{}|{}|{}|{}|{}",
                s.file.display(),
                s.name,
                s.kind.name(),
                s.line,
                s.start_byte,
                s.end_byte,
                calls.join(",")
            )
        })
        .collect();
    v.sort();
    v
}

fn write(root: &Path, rel: &str, body: &str) {
    let p = root.join(rel);
    if let Some(parent) = p.parent() {
        fs::create_dir_all(parent).unwrap();
    }
    fs::write(p, body).unwrap();
}

const A_RS: &str = r#"
fn helper() -> i32 { 1 }
fn caller() -> i32 { helper() }
struct Widget { n: i32 }
"#;

const B_RS: &str = r#"
fn other() { caller(); }
"#;

#[test]
fn incremental_refresh_equals_full_parse_across_add_change_delete() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db = root.join(".pirs/graph.db");
    write(root, "a.rs", A_RS);
    write(root, "b.rs", B_RS);

    // First open: cold build populates the cache.
    let mut store = GraphStore::open(&db, root).unwrap();
    let g1 = store.load_graph().unwrap();
    assert_eq!(
        fingerprint(&g1),
        fingerprint(&full_graph(root)),
        "cold store must equal full parse"
    );

    // Second open (fresh handle, warm cache): must still equal full parse, and
    // re-parse nothing because nothing changed.
    let mut store2 = GraphStore::open(&db, root).unwrap();
    let (_syms, stats) = store2.refresh().unwrap();
    assert_eq!(
        stats.reparsed, 0,
        "warm refresh must not re-parse: {stats:?}"
    );
    assert!(
        stats.unchanged >= 2,
        "both files seen as unchanged: {stats:?}"
    );
    let g2 = Graph::from_symbols(store2.refresh().unwrap().0);
    assert_eq!(fingerprint(&g2), fingerprint(&full_graph(root)));

    // CHANGE a.rs: add a new symbol. Only a.rs re-parses; result still == full.
    write(
        root,
        "a.rs",
        &format!("{A_RS}\nfn added() -> i32 {{ helper() }}\n"),
    );
    let mut store3 = GraphStore::open(&db, root).unwrap();
    let (syms3, stats3) = store3.refresh().unwrap();
    assert_eq!(
        stats3.reparsed, 1,
        "only the changed file re-parses: {stats3:?}"
    );
    let g3 = Graph::from_symbols(syms3);
    assert_eq!(
        fingerprint(&g3),
        fingerprint(&full_graph(root)),
        "after change, store must equal full parse"
    );
    assert!(!g3.symbol("added").is_empty(), "new symbol indexed");

    // DELETE b.rs: its symbols must drop; result still == full.
    fs::remove_file(root.join("b.rs")).unwrap();
    let mut store4 = GraphStore::open(&db, root).unwrap();
    let (syms4, stats4) = store4.refresh().unwrap();
    assert_eq!(stats4.deleted, 1, "the removed file is pruned: {stats4:?}");
    let g4 = Graph::from_symbols(syms4);
    assert_eq!(
        fingerprint(&g4),
        fingerprint(&full_graph(root)),
        "after delete, store must equal full parse"
    );
    assert!(g4.symbol("other").is_empty(), "deleted file's symbols gone");
}

#[test]
fn corrupt_db_is_recreated_not_fatal() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path();
    let db = root.join(".pirs/graph.db");
    write(root, "a.rs", A_RS);
    fs::create_dir_all(db.parent().unwrap()).unwrap();
    fs::write(&db, b"this is not a sqlite database").unwrap();

    // Must not panic or error: a garbage cache is wiped and rebuilt.
    let mut store = GraphStore::open(&db, root).unwrap();
    let g = store.load_graph().unwrap();
    assert_eq!(fingerprint(&g), fingerprint(&full_graph(root)));
}
