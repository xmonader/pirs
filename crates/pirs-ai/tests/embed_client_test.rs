//! `EmbeddingClient` against a canned OpenAI-compatible endpoint: proves it
//! parses the response, realigns out-of-order `index` fields to input order, and
//! surfaces non-2xx as an error instead of a bogus empty vector.

use std::io::{Read, Write};
use std::net::TcpListener;

use pirs_ai::EmbeddingClient;

/// Spawn a one-shot HTTP server that returns `body` (with the given status) for
/// the next connection, and return its base URL.
fn serve_once(status_line: &'static str, body: &'static str) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    std::thread::spawn(move || {
        if let Ok((mut sock, _)) = listener.accept() {
            let mut buf = [0u8; 4096];
            let _ = sock.read(&mut buf);
            let resp = format!(
                "HTTP/1.1 {status_line}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = sock.write_all(resp.as_bytes());
        }
    });
    format!("http://{addr}/v1")
}

#[tokio::test]
async fn parses_and_realigns_out_of_order_embeddings() {
    // Two inputs; server returns them in REVERSE index order on purpose.
    let body = r#"{"data":[
        {"index":1,"embedding":[9.0,9.0]},
        {"index":0,"embedding":[1.0,2.0]}
    ]}"#;
    let base = serve_once("200 OK", body);
    let client = EmbeddingClient::new(base, "test-model", None);
    let vecs = client
        .embed(&["first".into(), "second".into()])
        .await
        .expect("embed ok");
    assert_eq!(vecs.len(), 2);
    assert_eq!(vecs[0], vec![1.0, 2.0], "input 0 must map to index 0");
    assert_eq!(vecs[1], vec![9.0, 9.0], "input 1 must map to index 1");
}

#[tokio::test]
async fn empty_input_makes_no_request() {
    // Unroutable base: if embed() dialed out this would error; it must not.
    let client = EmbeddingClient::new("http://127.0.0.1:1/v1", "m", None);
    let vecs = client.embed(&[]).await.expect("empty is a no-op");
    assert!(vecs.is_empty());
}

#[tokio::test]
async fn non_success_status_is_an_error() {
    let base = serve_once("503 Service Unavailable", "model loading");
    let client = EmbeddingClient::new(base, "m", None);
    let err = client.embed(&["x".into()]).await.unwrap_err();
    assert!(
        format!("{err}").contains("503"),
        "expected http 503 error, got: {err}"
    );
}

#[tokio::test]
async fn count_mismatch_is_rejected() {
    // One input, server returns two vectors -> must not silently mis-map.
    let body = r#"{"data":[{"index":0,"embedding":[1.0]},{"index":1,"embedding":[2.0]}]}"#;
    let base = serve_once("200 OK", body);
    let client = EmbeddingClient::new(base, "m", None);
    assert!(client.embed(&["only-one".into()]).await.is_err());
}
