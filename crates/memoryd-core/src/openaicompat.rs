//! Generic OpenAI-compatible provider adapter.
//!
//! Provider-agnostic by design: it speaks the OpenAI wire shape
//! (`POST {base}/embeddings`, `POST {base}/chat/completions`,
//! `GET {base}/models`) against any configured base URL — api.openai.com,
//! Ollama's `/v1`, vLLM, LM Studio, llama.cpp — instead of shipping one
//! adapter per vendor. TLS comes from ureq/rustls; no system OpenSSL.
//!
//! Secret hygiene: the API key is sent on the wire and never logged or
//! embedded in error strings. Provider error bodies are truncated before they
//! enter `AdapterError` (they end up in `jobs.last_error`), so a misbehaving
//! endpoint cannot stuff arbitrary payloads into the store.

use std::time::Duration;

use crate::adapters::AdapterError;
use crate::config::OpenAiCompatConfig;

/// Per-call deadline. Kept under the 30s default job-lease visibility so an
/// in-flight embed cannot outlive its lease.
const CALL_TIMEOUT: Duration = Duration::from_secs(20);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
/// Reachability probes should fail fast; they gate nothing critical.
const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
/// Cap on provider response text echoed into error messages.
const ERROR_BODY_SNIPPET_CHARS: usize = 200;
/// Deterministic, model-agnostic instruction for dream consolidation.
const SUMMARIZE_SYSTEM_PROMPT: &str = "You consolidate raw engineering notes into one durable \
     memory. Reply with a single concise sentence that preserves every concrete fact (names, \
     versions, decisions). No preamble.";

/// See module docs. Construct via [`OpenAiCompatAdapter::from_config`].
pub struct OpenAiCompatAdapter {
    agent: ureq::Agent,
    base_url: String,
    api_key: Option<String>,
    embed_model: String,
    chat_model: String,
    usd_per_1k_prompt_tokens: f64,
}

impl std::fmt::Debug for OpenAiCompatAdapter {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Manual impl so the API key can never leak through {:?}.
        f.debug_struct("OpenAiCompatAdapter")
            .field("base_url", &self.base_url)
            .field("api_key", &self.api_key.as_ref().map(|_| "[REDACTED]"))
            .field("embed_model", &self.embed_model)
            .field("chat_model", &self.chat_model)
            .field("usd_per_1k_prompt_tokens", &self.usd_per_1k_prompt_tokens)
            .finish()
    }
}

impl Clone for OpenAiCompatAdapter {
    fn clone(&self) -> Self {
        Self {
            agent: self.agent.clone(),
            base_url: self.base_url.clone(),
            api_key: self.api_key.clone(),
            embed_model: self.embed_model.clone(),
            chat_model: self.chat_model.clone(),
            usd_per_1k_prompt_tokens: self.usd_per_1k_prompt_tokens,
        }
    }
}

impl OpenAiCompatAdapter {
    pub fn from_config(cfg: &OpenAiCompatConfig) -> Self {
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(CONNECT_TIMEOUT)
            .timeout(CALL_TIMEOUT)
            .build();
        Self {
            agent,
            base_url: cfg.base_url.trim_end_matches('/').to_string(),
            api_key: cfg.api_key.clone(),
            embed_model: cfg.embed_model.clone(),
            chat_model: cfg.chat_model.clone(),
            usd_per_1k_prompt_tokens: cfg.usd_per_1k_prompt_tokens,
        }
    }

    fn post_json(&self, path: &str, body: &serde_json::Value) -> Result<serde_json::Value, String> {
        let url = format!("{}{path}", self.base_url);
        let mut request = self
            .agent
            .post(&url)
            .set("Content-Type", "application/json");
        if let Some(key) = self.api_key.as_deref() {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        let response = match request.send_string(&body.to_string()) {
            Ok(response) => response,
            Err(ureq::Error::Status(status, response)) => {
                let snippet = snippet(&response.into_string().unwrap_or_default());
                return Err(format!("provider returned HTTP {status}: {snippet}"));
            }
            Err(ureq::Error::Transport(transport)) => {
                // Transport errors carry the URL but never request bodies/keys.
                return Err(format!("provider transport error: {transport}"));
            }
        };
        let text = response
            .into_string()
            .map_err(|err| format!("provider response read failed: {err}"))?;
        serde_json::from_str(&text)
            .map_err(|_| format!("provider returned non-JSON body: {}", snippet(&text)))
    }
}

impl crate::adapters::ProviderAdapter for OpenAiCompatAdapter {
    fn id(&self) -> &'static str {
        "openai_compat"
    }

    fn model_id(&self) -> &str {
        &self.embed_model
    }

    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }
        let body = serde_json::json!({
            "model": self.embed_model,
            "input": texts,
        });
        let response = self
            .post_json("/embeddings", &body)
            .map_err(AdapterError::Embed)?;
        let data = response
            .get("data")
            .and_then(serde_json::Value::as_array)
            .ok_or_else(|| AdapterError::Embed("embeddings response missing data".to_string()))?;
        if data.len() != texts.len() {
            return Err(AdapterError::Embed(format!(
                "embeddings response count mismatch: sent {}, got {}",
                texts.len(),
                data.len()
            )));
        }
        // The spec orders entries by `index`; re-sort defensively so a
        // permissive server cannot misalign vectors with their texts.
        let mut indexed: Vec<(usize, Vec<f32>)> = Vec::with_capacity(data.len());
        for (position, entry) in data.iter().enumerate() {
            let index = entry
                .get("index")
                .and_then(serde_json::Value::as_u64)
                .map(|index| index as usize)
                .unwrap_or(position);
            let vector: Vec<f32> = entry
                .get("embedding")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    AdapterError::Embed("embeddings entry missing embedding array".to_string())
                })?
                .iter()
                .map(|value| value.as_f64().unwrap_or(0.0) as f32)
                .collect();
            if vector.is_empty() {
                return Err(AdapterError::Embed(
                    "provider returned an empty embedding vector".to_string(),
                ));
            }
            indexed.push((index, vector));
        }
        indexed.sort_by_key(|(index, _)| *index);
        Ok(indexed.into_iter().map(|(_, vector)| vector).collect())
    }

    fn reachable(&self) -> bool {
        let url = format!("{}/models", self.base_url);
        let agent = ureq::AgentBuilder::new()
            .timeout_connect(PROBE_TIMEOUT)
            .timeout(PROBE_TIMEOUT)
            .build();
        let mut request = agent.get(&url);
        if let Some(key) = self.api_key.as_deref() {
            request = request.set("Authorization", &format!("Bearer {key}"));
        }
        match request.call() {
            Ok(_) => true,
            // Any HTTP status means a server answered; only transport
            // failures (refused, DNS, TLS, timeout) count as unreachable.
            Err(ureq::Error::Status(_, _)) => true,
            Err(ureq::Error::Transport(_)) => false,
        }
    }

    fn summarize(&self, texts: &[String]) -> Result<Option<String>, AdapterError> {
        if texts.is_empty() {
            return Ok(None);
        }
        let body = serde_json::json!({
            "model": self.chat_model,
            "temperature": 0,
            "messages": [
                { "role": "system", "content": SUMMARIZE_SYSTEM_PROMPT },
                { "role": "user", "content": texts.join("\n") },
            ],
        });
        let response = self
            .post_json("/chat/completions", &body)
            .map_err(AdapterError::Summarize)?;
        let content = response
            .get("choices")
            .and_then(|choices| choices.get(0))
            .and_then(|choice| choice.get("message"))
            .and_then(|message| message.get("content"))
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|content| !content.is_empty())
            .map(ToOwned::to_owned);
        match content {
            Some(content) => Ok(Some(content)),
            None => Err(AdapterError::Summarize(
                "chat response missing choices[0].message.content".to_string(),
            )),
        }
    }

    fn usd_per_1k_prompt_tokens(&self) -> f64 {
        self.usd_per_1k_prompt_tokens
    }
}

/// Char-boundary-safe truncation for provider text echoed into errors.
fn snippet(text: &str) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= ERROR_BODY_SNIPPET_CHARS {
        return trimmed.to_string();
    }
    let cut: String = trimmed.chars().take(ERROR_BODY_SNIPPET_CHARS).collect();
    format!("{cut}…")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::ProviderAdapter;
    use std::io::{Read, Write};
    use std::net::TcpListener;

    /// Minimal OpenAI-shaped mock: serves one canned response per accepted
    /// connection (the adapter sends Connection: close-equivalent one-shot
    /// requests) and records request lines + headers + bodies.
    fn spawn_mock(responses: Vec<(u16, String)>) -> (String, std::sync::mpsc::Receiver<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("mock binds");
        let addr = listener.local_addr().expect("mock addr");
        let (seen_tx, seen_rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (status, body) in responses {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let mut buffer = [0u8; 16 * 1024];
                let mut request = Vec::new();
                // Read until the full body arrived (Content-Length framing).
                while let Ok(read) = stream.read(&mut buffer) {
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&buffer[..read]);
                    let text = String::from_utf8_lossy(&request);
                    if let Some(headers_end) = text.find("\r\n\r\n") {
                        let content_length = text
                            .lines()
                            .find_map(|line| {
                                line.to_ascii_lowercase()
                                    .strip_prefix("content-length:")
                                    .map(|value| value.trim().parse::<usize>().unwrap_or(0))
                            })
                            .unwrap_or(0);
                        if request.len() >= headers_end + 4 + content_length {
                            break;
                        }
                    }
                }
                let _ = seen_tx.send(String::from_utf8_lossy(&request).into_owned());
                let reason = if status == 200 { "OK" } else { "Error" };
                let _ = write!(
                    stream,
                    "HTTP/1.1 {status} {reason}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
            }
        });
        (format!("http://{addr}"), seen_rx)
    }

    fn adapter_for(base_url: &str, api_key: Option<&str>) -> OpenAiCompatAdapter {
        OpenAiCompatAdapter::from_config(&OpenAiCompatConfig {
            base_url: base_url.to_string(),
            api_key: api_key.map(ToOwned::to_owned),
            embed_model: "test-embed".to_string(),
            chat_model: "test-chat".to_string(),
            usd_per_1k_prompt_tokens: 0.002,
        })
    }

    #[test]
    fn embed_parses_vectors_and_reorders_by_index() {
        let body = serde_json::json!({
            "data": [
                { "index": 1, "embedding": [0.5, 0.6] },
                { "index": 0, "embedding": [0.1, 0.2] },
            ]
        })
        .to_string();
        let (base, seen) = spawn_mock(vec![(200, body)]);
        let adapter = adapter_for(&base, Some("sekret-key"));

        let vectors = adapter
            .embed(&["first".to_string(), "second".to_string()])
            .expect("embed succeeds");
        assert_eq!(vectors, vec![vec![0.1, 0.2], vec![0.5, 0.6]]);

        let request = seen.recv().expect("mock saw request");
        assert!(request.starts_with("POST /embeddings HTTP/1.1"));
        assert!(
            request.contains("Authorization: Bearer sekret-key"),
            "bearer key sent"
        );
        assert!(request.contains("\"model\":\"test-embed\""));
    }

    #[test]
    fn embed_surfaces_status_errors_with_truncated_body() {
        let huge = "x".repeat(1000);
        let (base, _seen) = spawn_mock(vec![(429, format!("{{\"error\":\"{huge}\"}}"))]);
        let adapter = adapter_for(&base, None);

        let err = adapter
            .embed(&["text".to_string()])
            .expect_err("status error surfaces");
        let message = err.to_string();
        assert!(message.contains("HTTP 429"), "status visible: {message}");
        assert!(
            message.chars().count() < 300,
            "provider body truncated: {} chars",
            message.chars().count()
        );
    }

    #[test]
    fn embed_rejects_count_mismatch() {
        let body = serde_json::json!({
            "data": [ { "index": 0, "embedding": [0.1] } ]
        })
        .to_string();
        let (base, _seen) = spawn_mock(vec![(200, body)]);
        let adapter = adapter_for(&base, None);

        let err = adapter
            .embed(&["one".to_string(), "two".to_string()])
            .expect_err("mismatch rejected");
        assert!(err.to_string().contains("count mismatch"));
    }

    #[test]
    fn summarize_returns_chat_content() {
        let body = serde_json::json!({
            "choices": [ { "message": { "role": "assistant", "content": " WAL fixed. " } } ]
        })
        .to_string();
        let (base, seen) = spawn_mock(vec![(200, body)]);
        let adapter = adapter_for(&base, None);

        let summary = adapter
            .summarize(&["a".to_string(), "b".to_string()])
            .expect("summarize succeeds");
        assert_eq!(summary.as_deref(), Some("WAL fixed."));

        let request = seen.recv().expect("mock saw request");
        assert!(request.starts_with("POST /chat/completions HTTP/1.1"));
        assert!(request.contains("\"model\":\"test-chat\""));
    }

    #[test]
    fn reachable_true_for_any_http_status_false_for_refused() {
        let (base, _seen) = spawn_mock(vec![(401, "{}".to_string())]);
        let adapter = adapter_for(&base, None);
        assert!(adapter.reachable(), "401 still means a server answered");

        // Bind-then-drop guarantees a port nothing is listening on.
        let dead = TcpListener::bind("127.0.0.1:0").expect("binds");
        let dead_addr = dead.local_addr().expect("addr");
        drop(dead);
        let unreachable = adapter_for(&format!("http://{dead_addr}"), None);
        assert!(!unreachable.reachable());
    }

    #[test]
    fn debug_never_prints_the_api_key() {
        let adapter = adapter_for("http://127.0.0.1:1", Some("super-secret-key"));
        let debug = format!("{adapter:?}");
        assert!(!debug.contains("super-secret-key"));
        assert!(debug.contains("[REDACTED]"));
    }

    #[test]
    fn adapter_surface_is_semantic_with_configured_price() {
        let adapter = adapter_for("http://127.0.0.1:1", None);
        assert_eq!(adapter.id(), "openai_compat");
        assert_eq!(adapter.model_id(), "test-embed");
        assert!(adapter.embeds_semantically());
        assert_eq!(adapter.usd_per_1k_prompt_tokens(), 0.002);
        assert!(
            adapter
                .embed(&[])
                .expect("empty input is a no-op")
                .is_empty(),
            "no network call for empty input"
        );
    }
}
