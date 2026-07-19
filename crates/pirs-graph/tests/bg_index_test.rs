//! The background indexer's two product-grade guarantees:
//!   1. it fills the whole index on its own (behind BM25, off the search path);
//!   2. a kill mid-build resumes from the last checkpoint instead of re-embedding
//!      everything — the property the old store-at-the-end design lacked.
//!
//! Driven against a mock `/v1/embeddings` server that counts how many symbols
//! were actually embedded, so "no rework on resume" is a measured fact.

use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use pirs_ai::EmbeddingClient;
use pirs_graph::BackgroundIndexer;
use pirs_graph::GraphStore;

/// Mock embeddings server. Returns a 4-dim vector per input, and counts every
/// non-probe input it embeds into `embed_count`. Serves until the process exits.
fn spawn_counting_embedder(embed_count: Arc<AtomicUsize>, delay_ms: u64) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut sock) = conn else { break };
            if delay_ms > 0 {
                std::thread::sleep(Duration::from_millis(delay_ms));
            }
            let mut buf = vec![0u8; 1 << 20];
            let n = sock.read(&mut buf).unwrap_or(0);
            let req = String::from_utf8_lossy(&buf[..n]);
            let body = req.split_once("\r\n\r\n").map(|(_, b)| b).unwrap_or("");
            let v: serde_json::Value =
                serde_json::from_str(body).unwrap_or(serde_json::Value::Null);
            let inputs = v
                .get("input")
                .and_then(|i| i.as_array())
                .cloned()
                .unwrap_or_default();
            let mut data = Vec::new();
            for (i, s) in inputs.iter().enumerate() {
                let text = s.as_str().unwrap_or("");
                if text != "dimension probe" {
                    embed_count.fetch_add(1, Ordering::SeqCst);
                }
                data.push(serde_json::json!({"index": i, "embedding": [0.1, 0.2, 0.3, 0.4]}));
            }
            let payload = serde_json::json!({ "data": data }).to_string();
            let resp = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                payload.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}/v1")
}

fn count(db: &Path, root: &Path) -> usize {
    GraphStore::open(db, root)
        .and_then(|s| s.embedding_count())
        .unwrap_or(0)
}

/// Write a repo with `n` distinct functions across a few files -> `n` symbols,
/// enough to span several checkpoint batches (batch = 64).
fn write_repo(root: &Path, n: usize) {
    for f in 0..4 {
        let mut src = String::new();
        for i in (f..n).step_by(4) {
            src.push_str(&format!("fn func_{i}() -> i32 {{ {i} }}\n"));
        }
        std::fs::write(root.join(format!("mod{f}.rs")), src).unwrap();
    }
}

async fn poll_until<F: Fn() -> bool>(cond: F, timeout: Duration) -> bool {
    let start = Instant::now();
    while start.elapsed() < timeout {
        if cond() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    cond()
}

#[tokio::test]
async fn fills_whole_index_behind_the_search_path() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write_repo(&root, 150);
    let db = root.join(".pirs/graph.db");

    let embeds = Arc::new(AtomicUsize::new(0));
    let base = spawn_counting_embedder(Arc::clone(&embeds), 0);
    let embedder = EmbeddingClient::new(base, "mock", None);

    let idx = BackgroundIndexer::new(root.clone(), db.clone(), embedder, 2000);
    let handle = tokio::spawn(idx.run());

    let filled = poll_until(|| count(&db, &root) == 150, Duration::from_secs(20)).await;
    handle.abort();
    assert!(
        filled,
        "indexer should embed all 150 symbols; got {}",
        count(&db, &root)
    );
    // Each symbol embedded exactly once (probes excluded).
    assert_eq!(embeds.load(Ordering::SeqCst), 150, "no redundant embeds");
}

#[tokio::test]
async fn resumes_after_kill_without_reembedding() {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().to_path_buf();
    write_repo(&root, 150);
    let db = root.join(".pirs/graph.db");

    let embeds = Arc::new(AtomicUsize::new(0));
    // Slow the mock so the build is catchable mid-flight (each batch ~60ms).
    let base = spawn_counting_embedder(Arc::clone(&embeds), 60);

    // First indexer: run until at least one checkpoint has landed, then "kill".
    let idx1 = BackgroundIndexer::new(
        root.clone(),
        db.clone(),
        EmbeddingClient::new(base.clone(), "mock", None),
        2000,
    );
    let h1 = tokio::spawn(idx1.run());
    poll_until(
        || (64..150).contains(&count(&db, &root)),
        Duration::from_secs(20),
    )
    .await;
    h1.abort();
    // Let any in-flight request finish counting before we snapshot.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let after_kill = count(&db, &root);
    let embeds_run1 = embeds.load(Ordering::SeqCst);
    assert!(
        (64..150).contains(&after_kill),
        "test wants a partial, checkpointed build before the kill, got {after_kill}"
    );

    // Second indexer resumes and finishes.
    let idx2 = BackgroundIndexer::new(
        root.clone(),
        db.clone(),
        EmbeddingClient::new(base, "mock", None),
        2000,
    );
    let h2 = tokio::spawn(idx2.run());
    let filled = poll_until(|| count(&db, &root) == 150, Duration::from_secs(20)).await;
    h2.abort();
    assert!(
        filled,
        "resumed indexer should finish; got {}",
        count(&db, &root)
    );

    // The exact guarantee: run 2 embeds ONLY the symbols run 1 hadn't checkpointed
    // — not the whole repo. A from-scratch restart would embed 150 again.
    let embeds_run2 = embeds.load(Ordering::SeqCst) - embeds_run1;
    assert_eq!(
        embeds_run2,
        150 - after_kill,
        "resume must embed only the remaining {} pending, not re-index all 150",
        150 - after_kill
    );
}
