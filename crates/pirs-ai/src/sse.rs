use futures_util::StreamExt;

pub struct SseStream {
    inner: futures_util::stream::BoxStream<'static, Result<String, crate::AiError>>,
}

impl SseStream {
    pub fn new(response: reqwest::Response) -> Self {
        let stream = response
            .bytes_stream()
            .scan(Vec::new(), |buf: &mut Vec<u8>, chunk| {
                let events = match chunk {
                    Ok(bytes) => feed_events(buf, &bytes)
                        .into_iter()
                        .map(|e| Ok(e.data))
                        .collect(),
                    Err(e) => vec![Err(crate::AiError::Network(e))],
                };
                std::future::ready(Some(futures_util::stream::iter(events)))
            })
            .flatten()
            .boxed();
        SseStream { inner: stream }
    }

    pub async fn next(&mut self) -> Option<Result<String, crate::AiError>> {
        self.inner.next().await
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct SseEvent {
    pub event: Option<String>,
    pub data: String,
}

pub struct SseEventStream {
    inner: futures_util::stream::BoxStream<'static, Result<SseEvent, crate::AiError>>,
    buf: std::sync::Arc<std::sync::Mutex<Vec<u8>>>,
}

impl SseEventStream {
    pub fn new(response: reqwest::Response) -> Self {
        let shared: std::sync::Arc<std::sync::Mutex<Vec<u8>>> =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let shared2 = std::sync::Arc::clone(&shared);
        let stream = response
            .bytes_stream()
            .scan(Vec::new(), move |buf: &mut Vec<u8>, chunk| {
                let events = match chunk {
                    Ok(bytes) => feed_events(buf, &bytes)
                        .into_iter()
                        .map(Ok)
                        .collect::<Vec<_>>(),
                    Err(e) => vec![Err(crate::AiError::Network(e))],
                };
                *shared2.lock().unwrap() = buf.clone();
                std::future::ready(Some(futures_util::stream::iter(events)))
            })
            .flatten()
            .boxed();
        SseEventStream {
            inner: stream,
            buf: shared,
        }
    }

    /// Unconsumed bytes left after the stream ends (for EOF salvage).
    pub fn remaining(&self) -> Vec<u8> {
        self.buf.lock().unwrap().clone()
    }

    pub async fn next(&mut self) -> Option<Result<SseEvent, crate::AiError>> {
        self.inner.next().await
    }
}

/// Salvage a trailing frame that lacks the terminating blank line (EOF cut).
#[doc(hidden)]
pub fn salvage_tail(buf: &[u8]) -> Option<SseEvent> {
    if buf.is_empty() {
        return None;
    }
    let body = String::from_utf8_lossy(buf);
    let body = body.trim();
    if body.is_empty() {
        return None;
    }
    let mut data = String::new();
    let mut event = None;
    for line in body.lines() {
        if let Some(rest) = line.strip_prefix("event:") {
            event = Some(rest.trim().to_string());
        } else if let Some(rest) = line.strip_prefix("data:") {
            if !data.is_empty() {
                data.push('\n');
            }
            data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
        }
    }
    if data.is_empty() {
        None
    } else {
        Some(SseEvent { event, data })
    }
}

#[doc(hidden)]
pub fn feed(buf: &mut Vec<u8>, bytes: &[u8]) -> Vec<String> {
    feed_events(buf, bytes)
        .into_iter()
        .map(|e| e.data)
        .collect()
}

#[doc(hidden)]
pub fn feed_events(buf: &mut Vec<u8>, bytes: &[u8]) -> Vec<SseEvent> {
    buf.extend_from_slice(bytes);
    let mut events = Vec::new();
    while let Some((pos, delim_len)) = find_event_boundary(buf) {
        let raw: Vec<u8> = buf.drain(..pos + delim_len).collect();
        let body = String::from_utf8_lossy(&raw[..pos]);
        let body = body.trim();
        if body.is_empty() {
            continue;
        }
        let mut data = String::new();
        let mut event = None;
        for line in body.lines() {
            if let Some(rest) = line.strip_prefix("event:") {
                event = Some(rest.trim().to_string());
            } else if let Some(rest) = line.strip_prefix("data:") {
                if !data.is_empty() {
                    data.push('\n');
                }
                data.push_str(rest.strip_prefix(' ').unwrap_or(rest));
            }
        }
        if !data.is_empty() {
            events.push(SseEvent { event, data });
        }
    }
    events
}

fn find_event_boundary(buf: &[u8]) -> Option<(usize, usize)> {
    let crlf = find_subsequence(buf, b"\r\n\r\n").map(|p| (p, 4));
    let lf = find_subsequence(buf, b"\n\n").map(|p| (p, 2));
    match (crlf, lf) {
        (Some(a), Some(b)) => Some(if a.0 <= b.0 { a } else { b }),
        (Some(a), None) => Some(a),
        (None, Some(b)) => Some(b),
        (None, None) => None,
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn boundary_lf() {
        assert_eq!(find_event_boundary(b"a\n\nb"), Some((1, 2)));
    }

    #[test]
    fn boundary_crlf() {
        assert_eq!(find_event_boundary(b"a\r\n\r\nb"), Some((1, 4)));
    }

    #[test]
    fn none() {
        assert_eq!(find_event_boundary(b"a\nb"), None);
    }

    #[test]
    fn parse_single_event() {
        let mut buf = b"data: hello\n\nrest".to_vec();
        let (pos, len) = find_event_boundary(&buf).unwrap();
        let raw: Vec<u8> = buf.drain(..pos + len).collect();
        assert_eq!(raw, b"data: hello\n\n");
        assert_eq!(buf, b"rest");
    }
}
