use pirs_ai::sse::feed;

#[test]
fn multibyte_char_split_across_chunks() {
    let bytes = "data: {\"a\":\"héllo — 日本語 👋\"}\n\ndata: [DONE]\n\n".as_bytes();
    let mut buf = Vec::new();
    let mut events = Vec::new();
    for split in 1..bytes.len() {
        buf.clear();
        events.clear();
        events.extend(feed(&mut buf, &bytes[..split]));
        events.extend(feed(&mut buf, &bytes[split..]));
        assert_eq!(events.len(), 2, "split at {split}");
        assert_eq!(
            events[0], "{\"a\":\"héllo — 日本語 👋\"}",
            "split at {split}"
        );
        assert_eq!(events[1], "[DONE]");
        assert!(buf.is_empty(), "split at {split}");
    }
}

#[test]
fn crlf_and_mixed_boundaries() {
    let mut buf = Vec::new();
    let events = feed(&mut buf, b"data: one\r\n\r\ndata: two\n\n");
    assert_eq!(events, vec!["one", "two"]);
}
