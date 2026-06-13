//! Minimal client for a running `serve`'s `POST /v1/recall`.
//!
//! The prompt hook uses this to get **warm-model** semantic recall: `serve`
//! keeps the embed model resident (see `localembed`'s `OnceLock` cache), so a
//! recall round-trips in ~tens of ms, versus ~seconds to cold-load the model in
//! the hook's short-lived process. Any error (serve down, auth failure, timeout,
//! malformed body) is the caller's signal to fall back to local recall, so the
//! hook never blocks on it. Bounded timeouts keep a stalled serve from delaying
//! prompt submission. Loopback-only by construction (the caller passes serve's
//! own bind address); no public-internet calls.

use std::time::Duration;

/// Connect deadline: a down serve refuses fast on loopback, so this only bounds
/// pathological cases (e.g. a half-open socket).
const CONNECT_TIMEOUT: Duration = Duration::from_secs(1);
/// Read deadline: a warm serve answers in ~tens of ms; this caps the wait so a
/// busy/cold serve degrades to local recall instead of stalling the prompt.
const READ_TIMEOUT: Duration = Duration::from_secs(3);

/// One recall hit surfaced to the hook: the durable kind plus its content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeHit {
    pub kind: String,
    pub content: String,
}

/// Semantic-recall the prompt against a running serve. Returns up to `k` hits
/// from the `results` array of `POST {base_url}/v1/recall`. `Err` means the
/// caller should fall back to local recall — it is never a reason to fail the
/// hook. The bearer token, when present, is sent only as an `Authorization`
/// header and never echoed into the returned error.
pub fn recall_via_serve(
    base_url: &str,
    token: Option<&str>,
    query: &str,
    k: usize,
) -> Result<Vec<ServeHit>, String> {
    let agent = ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(READ_TIMEOUT)
        .build();
    let body = serde_json::json!({ "query": query, "k": k, "semantic": true });
    let mut request = agent
        .post(&format!("{base_url}/v1/recall"))
        .set("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.set("Authorization", &format!("Bearer {token}"));
    }
    let response = match request.send_string(&body.to_string()) {
        Ok(response) => response,
        // Any HTTP status (401/404/5xx) or transport failure means "fall back".
        // The body is not read into the error so a misbehaving endpoint cannot
        // stuff payloads into the hook's stderr.
        Err(ureq::Error::Status(status, _)) => return Err(format!("serve recall HTTP {status}")),
        Err(ureq::Error::Transport(_)) => return Err("serve unreachable".to_string()),
    };
    let text = response
        .into_string()
        .map_err(|err| format!("serve recall body read failed: {err}"))?;
    let value: serde_json::Value = serde_json::from_str(&text)
        .map_err(|err| format!("serve recall body parse failed: {err}"))?;
    Ok(parse_results(&value, k))
}

/// Extract `(kind, content)` hits from a recall response's `results` array,
/// capped at `k`. Missing/malformed entries are skipped rather than failing —
/// a partial answer still beats no recall.
fn parse_results(value: &serde_json::Value, k: usize) -> Vec<ServeHit> {
    let Some(results) = value.get("results").and_then(serde_json::Value::as_array) else {
        return Vec::new();
    };
    results
        .iter()
        .filter_map(|hit| {
            let kind = hit.get("kind").and_then(serde_json::Value::as_str)?;
            let content = hit.get("content").and_then(serde_json::Value::as_str)?;
            Some(ServeHit {
                kind: kind.to_string(),
                content: content.to_string(),
            })
        })
        .take(k)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// One-shot HTTP mock: serves a single canned response and records the
    /// request line + headers + body for assertions.
    fn spawn_mock(status: u16, body: String) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock binds");
        let addr = listener.local_addr().expect("mock addr");
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut buf = [0u8; 8192];
                let mut req = Vec::new();
                while let Ok(n) = stream.read(&mut buf) {
                    if n == 0 {
                        break;
                    }
                    req.extend_from_slice(&buf[..n]);
                    let text = String::from_utf8_lossy(&req);
                    if let Some(end) = text.find("\r\n\r\n") {
                        let len = text
                            .lines()
                            .find_map(|l| {
                                l.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|v| v.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if req.len() >= end + 4 + len {
                            break;
                        }
                    }
                }
                let _ = tx.send(String::from_utf8_lossy(&req).into_owned());
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}"), rx)
    }

    #[test]
    fn parses_results_and_sends_semantic_query_with_bearer() {
        let body = serde_json::json!({
            "results": [
                { "memory_id": "m1", "kind": "preference", "content": "prefers tabs" },
                { "memory_id": "m2", "kind": "decision", "content": "use sqlite" },
            ]
        })
        .to_string();
        let (base, seen) = spawn_mock(200, body);

        let hits = recall_via_serve(&base, Some("sekret"), "tabs or spaces", 5).expect("ok");
        assert_eq!(
            hits,
            vec![
                ServeHit {
                    kind: "preference".to_string(),
                    content: "prefers tabs".to_string()
                },
                ServeHit {
                    kind: "decision".to_string(),
                    content: "use sqlite".to_string()
                },
            ]
        );

        let request = seen.recv().expect("mock saw request");
        assert!(request.starts_with("POST /v1/recall HTTP/1.1"));
        assert!(
            request.contains("Authorization: Bearer sekret"),
            "token sent"
        );
        assert!(
            request.contains("\"semantic\":true"),
            "asks for semantic recall"
        );
    }

    #[test]
    fn caps_results_at_k() {
        let body = serde_json::json!({
            "results": [
                { "kind": "a", "content": "1" },
                { "kind": "b", "content": "2" },
                { "kind": "c", "content": "3" },
            ]
        })
        .to_string();
        let (base, _seen) = spawn_mock(200, body);
        let hits = recall_via_serve(&base, None, "q", 2).expect("ok");
        assert_eq!(hits.len(), 2, "honors k");
    }

    #[test]
    fn empty_results_is_ok_and_empty() {
        let (base, _seen) = spawn_mock(200, "{\"results\":[]}".to_string());
        let hits = recall_via_serve(&base, None, "q", 3).expect("ok");
        assert!(hits.is_empty(), "no hits is a valid authoritative answer");
    }

    #[test]
    fn http_error_status_is_err_for_fallback() {
        let (base, _seen) = spawn_mock(401, "{\"error\":\"unauthorized\"}".to_string());
        let err = recall_via_serve(&base, None, "q", 3).expect_err("401 surfaces");
        assert!(err.contains("401"), "status visible: {err}");
    }

    #[test]
    fn unreachable_serve_is_err_for_fallback() {
        // Bind then drop to guarantee a refused port.
        let dead = TcpListener::bind("127.0.0.1:0").expect("binds");
        let addr = dead.local_addr().expect("addr");
        drop(dead);
        let err = recall_via_serve(&format!("http://{addr}"), None, "q", 3)
            .expect_err("refused connection surfaces");
        assert!(err.contains("unreachable"), "transport error mapped: {err}");
    }
}
