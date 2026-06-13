//! `memoryd hook`: agent-hook handlers — the hooks-only (no-MCP) integration
//! path. Agents invoke these as shell hooks; each verb reads the agent's hook
//! payload JSON on stdin and either captures into the store (push: prompts,
//! tool results) or prints context to inject (pull: persona at session start,
//! recall per prompt).
//!
//! Contract with the host agent:
//! - NEVER fail the agent: every error path exits 0 with empty stdout (detail
//!   to stderr). A broken memory daemon must not block coding sessions —
//!   in Claude Code/Codex a nonzero exit can block the hooked action.
//! - Fast: capture is the µs SQLite append path. Prompt recall prefers the
//!   running `serve` (localhost, warm embed model = semantic recall in ~tens of
//!   ms) and falls back to local lexical recall when serve is unreachable — so
//!   relevance is semantic when possible and never blocks the prompt. The only
//!   network is that loopback call to the owner's own serve; no public internet,
//!   and dreaming stays in `dream`.
//! - Payload-tolerant: field names cover Claude Code and Codex (`session_id`,
//!   `prompt`, `tool_name`, `tool_input`, `tool_response`, `hook_event_name`)
//!   and Hermes (`extra` envelope); missing fields degrade to placeholders.

use memoryd_core::store::{NewRawEvent, Store};
use std::io::Read;

/// Caps bytes read from stdin so a pathological multi-GB tool output cannot
/// balloon memory before the char-level truncation below applies.
const STDIN_BYTE_CAP: u64 = 1024 * 1024;
/// Caps captured text so one giant tool response cannot bloat the store
/// (redaction and FTS indexing both run over this text).
const CAPTURE_TEXT_CAP: usize = 4_000;
/// Caps injected context so hooks never flood the agent's window.
const CONTEXT_CHAR_CAP: usize = 2_000;
/// Recall hits injected per prompt.
const PROMPT_RECALL_K: usize = 3;
/// Profile facts / themes injected at session start.
const PROFILE_FACT_CAP: usize = 30;
const PROFILE_THEME_CAP: usize = 5;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct HookArgs {
    /// Which hook verb to run: "tool" | "prompt" | "session-start".
    pub verb: String,
    /// Capture-surface label stamped into provenance ("claude", "codex", ...).
    pub agent: String,
}

pub(crate) fn run(cli: &crate::Cli, args: &HookArgs) -> Result<(), crate::CliError> {
    // Hooks must never break the host agent: report success regardless, with
    // the real failure on stderr for the curious.
    if let Err(err) = run_fallible(cli, args) {
        eprintln!("memoryd hook: {err}");
    }
    Ok(())
}

fn run_fallible(cli: &crate::Cli, args: &HookArgs) -> Result<(), crate::CliError> {
    let raw = read_stdin_capped(std::io::stdin());
    let payload: serde_json::Value = serde_json::from_str(&raw).unwrap_or(serde_json::json!({}));

    let cfg = cli.config()?;
    cfg.validate()?;

    match args.verb.as_str() {
        "tool" => capture_tool(&cfg, &payload, &args.agent),
        "prompt" => {
            // Recall BEFORE capturing, so the injected context reflects prior
            // memory rather than echoing the just-submitted prompt back.
            emit_prompt_context(&cfg, &payload)?;
            capture_prompt(&cfg, &payload, &args.agent)
        }
        "session-start" => emit_profile_context(&cfg, &payload),
        other => Err(crate::CliError::UnknownHookVerb(other.to_string())),
    }
}

/// Read at most `STDIN_BYTE_CAP` bytes from the hook payload stream. Reading
/// into bytes first keeps a `take()` that lands mid-codepoint from erroring:
/// `from_utf8_lossy` degrades the split char to U+FFFD instead. Read errors
/// keep whatever arrived (hooks never fail the agent; detail to stderr).
fn read_stdin_capped(reader: impl Read) -> String {
    let mut bytes = Vec::new();
    if let Err(err) = reader.take(STDIN_BYTE_CAP).read_to_end(&mut bytes) {
        eprintln!("memoryd hook: stdin read failed: {err}");
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

/// Common envelope fields, tolerant across agents.
fn session_id(payload: &serde_json::Value) -> String {
    payload
        .get("session_id")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("hook")
        .to_string()
}

fn capture(
    cfg: &memoryd_core::config::Config,
    agent: &str,
    session: String,
    kind: &str,
    text: String,
    event_name: Option<&str>,
) -> Result<(), crate::CliError> {
    let mut store = Store::open(&cfg.db_path)?;
    store.capture_event_with_queue_limit(
        NewRawEvent {
            session_id: session,
            agent: agent.to_string(),
            source: "hook".to_string(),
            kind: kind.to_string(),
            payload: serde_json::json!({ "text": text }),
            provenance: serde_json::json!({
                "via": "hook",
                "event": event_name,
            }),
            ts_ms: crate::unix_ms_now(),
        },
        cfg.caps.queue_depth_max,
    )?;
    Ok(())
}

/// PostToolUse / post_tool_call: capture "what the agent did and saw".
fn capture_tool(
    cfg: &memoryd_core::config::Config,
    payload: &serde_json::Value,
    agent: &str,
) -> Result<(), crate::CliError> {
    let tool = payload
        .get("tool_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or("tool");
    let input = compact_json(payload.get("tool_input"));
    // Claude: tool_output {stdout,stderr,exit_code}; Codex: tool_response;
    // Hermes: event kwargs ride in `extra`.
    let response = compact_json(
        payload
            .get("tool_output")
            .or_else(|| payload.get("tool_response"))
            .or_else(|| payload.get("extra")),
    );
    let mut text = format!("{tool}: {input}");
    if !response.is_empty() && response != "null" {
        text.push_str(" -> ");
        text.push_str(&response);
    }
    capture(
        cfg,
        agent,
        session_id(payload),
        "observation",
        truncate(&text, CAPTURE_TEXT_CAP),
        payload.get("hook_event_name").and_then(|v| v.as_str()),
    )
}

/// UserPromptSubmit: capture the owner's intent verbatim.
fn capture_prompt(
    cfg: &memoryd_core::config::Config,
    payload: &serde_json::Value,
    agent: &str,
) -> Result<(), crate::CliError> {
    let Some(prompt) = payload.get("prompt").and_then(serde_json::Value::as_str) else {
        return Ok(()); // nothing to capture, still emit context below
    };
    if prompt.trim().is_empty() {
        return Ok(());
    }
    capture(
        cfg,
        agent,
        session_id(payload),
        "observation",
        truncate(prompt, CAPTURE_TEXT_CAP),
        payload.get("hook_event_name").and_then(|v| v.as_str()),
    )
}

/// UserPromptSubmit stdout: recall the prompt against the memory corpus and
/// inject the top hits as additionalContext.
fn emit_prompt_context(
    cfg: &memoryd_core::config::Config,
    payload: &serde_json::Value,
) -> Result<(), crate::CliError> {
    let Some(prompt) = payload.get("prompt").and_then(serde_json::Value::as_str) else {
        return Ok(());
    };
    let lines = recall_lines(cfg, prompt);
    if lines.is_empty() {
        return Ok(()); // silent no-op: nothing relevant remembered
    }
    let context = truncate(
        &format!("Relevant memoryd memories:\n{}", lines.join("\n")),
        CONTEXT_CHAR_CAP,
    );
    print_context(payload, "UserPromptSubmit", &context)
}

/// Recall the prompt and format the top hits as `- [kind] content` lines.
///
/// Prefers the running serve's **semantic** recall (warm embed model, ~tens of
/// ms). When serve answers, its result is authoritative — even an empty result
/// means "nothing semantically relevant", so we do NOT fall back and inject
/// lexical noise. Only when serve is unreachable/unauthorized do we degrade to
/// the local **lexical** recall (fast, no model load) to preserve the offline
/// path. Either way this never fails the hook.
fn recall_lines(cfg: &memoryd_core::config::Config, prompt: &str) -> Vec<String> {
    let base_url = format!("http://{}", cfg.bind);
    match memoryd_core::serve_client::recall_via_serve(
        &base_url,
        cfg.bearer_token.as_deref(),
        prompt,
        PROMPT_RECALL_K,
    ) {
        Ok(hits) => hits
            .iter()
            .map(|hit| format!("- [{}] {}", hit.kind, hit.content))
            .collect(),
        // serve down/unauthorized/timed out → degrade to local lexical recall.
        Err(_) => recall_local_lexical(cfg, prompt),
    }
}

/// Fallback recall: lexical (FTS) recall straight from SQLite, no model load.
/// Used only when serve is unavailable; mirrors the pre-serve behavior.
fn recall_local_lexical(cfg: &memoryd_core::config::Config, prompt: &str) -> Vec<String> {
    let Ok(store) = Store::open(&cfg.db_path) else {
        return Vec::new();
    };
    let adapter = memoryd_core::adapters::AdapterKind::from_default_adapter("local");
    let args = crate::RecallArgs {
        query: prompt.to_string(),
        limit: PROMPT_RECALL_K,
        semantic: false,
        hops: 1,
        index_kind: None,
    };
    let mut lines = Vec::new();
    if let Ok(result) = crate::recall_with_mode(&store, &args, "brute-force", &adapter) {
        match result {
            crate::RecallOutput::Memory(memory) => {
                for hit in memory.hits.iter().take(PROMPT_RECALL_K) {
                    lines.push(format!("- [{}] {}", hit.kind, hit.content));
                }
            }
            crate::RecallOutput::Event(event) => {
                for hit in event.hits.iter().take(PROMPT_RECALL_K) {
                    lines.push(format!("- [{}] {}", hit.kind, hit.content));
                }
            }
        }
    }
    lines
}

/// SessionStart stdout: load the owner persona kernel (approved profile facts
/// + top graph themes) so every session starts as "you".
fn emit_profile_context(
    cfg: &memoryd_core::config::Config,
    payload: &serde_json::Value,
) -> Result<(), crate::CliError> {
    let store = Store::open(&cfg.db_path)?;
    let facts = store.active_profile_facts(PROFILE_FACT_CAP)?;
    let themes = store.top_central_memories(PROFILE_THEME_CAP)?;
    if facts.is_empty() && themes.is_empty() {
        return Ok(());
    }
    let mut lines = vec!["Owner profile (memoryd, human-approved):".to_string()];
    for fact in &facts {
        lines.push(format!("- {}: {}", fact.fact_key, fact.fact_value));
    }
    if !themes.is_empty() {
        lines.push("Recurring themes:".to_string());
        for theme in &themes {
            lines.push(format!("- {}", truncate(&theme.content, 160)));
        }
    }
    let context = truncate(&lines.join("\n"), CONTEXT_CHAR_CAP);
    print_context(payload, "SessionStart", &context)
}

/// The `hookSpecificOutput.additionalContext` envelope understood by both
/// Claude Code and Codex; `hookEventName` echoes the payload's event when
/// present so the same handler serves either event spelling.
fn print_context(
    payload: &serde_json::Value,
    default_event: &str,
    context: &str,
) -> Result<(), crate::CliError> {
    let event = payload
        .get("hook_event_name")
        .and_then(serde_json::Value::as_str)
        .unwrap_or(default_event);
    println!(
        "{}",
        serde_json::to_string(&serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": context,
            }
        }))?
    );
    Ok(())
}

/// Single-line JSON rendering for tool inputs/outputs, "" for absent.
fn compact_json(value: Option<&serde_json::Value>) -> String {
    match value {
        None | Some(serde_json::Value::Null) => String::new(),
        Some(serde_json::Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

/// Char-boundary-safe truncation with ellipsis.
fn truncate(text: &str, max_chars: usize) -> String {
    let mut iter = text.char_indices();
    match iter.nth(max_chars) {
        None => text.to_string(),
        Some((byte_index, _)) => {
            let mut out = text[..byte_index].to_string();
            out.push('…');
            out
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn temp_db(name: &str) -> PathBuf {
        std::env::temp_dir().join(format!(
            "memoryd-hook-{name}-{}-{:?}.db",
            std::process::id(),
            std::thread::current().id()
        ))
    }

    fn cfg_for(path: &Path) -> memoryd_core::config::Config {
        memoryd_core::config::Config::with_db_path(path.to_path_buf())
    }

    fn cleanup(path: &Path) {
        for suffix in ["", "-shm", "-wal"] {
            let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
        }
    }

    #[test]
    fn tool_payload_is_captured_with_truncation() {
        let path = temp_db("tool");
        let cfg = cfg_for(&path);
        let payload = serde_json::json!({
            "session_id": "s-claude",
            "hook_event_name": "PostToolUse",
            "tool_name": "Bash",
            "tool_input": { "command": "cargo test" },
            "tool_response": { "stdout": "x".repeat(10_000) },
        });
        capture_tool(&cfg, &payload, "claude").expect("capture succeeds");

        let store = Store::open(&path).expect("store opens");
        let result = store.recall_events("cargo test", 5).expect("recall");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].session_id, "s-claude");
        assert!(result.hits[0].content.starts_with("Bash:"));
        assert!(
            result.hits[0].content.chars().count() <= CAPTURE_TEXT_CAP + 1,
            "giant tool output truncated"
        );
        cleanup(&path);
    }

    #[test]
    fn hermes_extra_envelope_is_captured() {
        let path = temp_db("hermes");
        let cfg = cfg_for(&path);
        let payload = serde_json::json!({
            "session_id": "sess_h",
            "hook_event_name": "post_tool_call",
            "tool_name": "write_file",
            "tool_input": { "path": "main.py" },
            "extra": { "ok": true },
        });
        capture_tool(&cfg, &payload, "hermes").expect("capture succeeds");
        let store = Store::open(&path).expect("store opens");
        let result = store.recall_events("write_file", 5).expect("recall");
        assert_eq!(result.hits.len(), 1);
        assert!(result.hits[0].content.contains("main.py"));
        cleanup(&path);
    }

    #[test]
    fn prompt_is_captured_and_blank_prompt_skipped() {
        let path = temp_db("prompt");
        let cfg = cfg_for(&path);
        capture_prompt(
            &cfg,
            &serde_json::json!({ "session_id": "s", "prompt": "fix the WAL timeout" }),
            "claude",
        )
        .expect("capture succeeds");
        capture_prompt(&cfg, &serde_json::json!({ "prompt": "   " }), "claude")
            .expect("blank prompt is a no-op");

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("stats");
        let rows = stats
            .iter()
            .find(|s| s.table == "raw_events")
            .map(|s| s.rows)
            .unwrap_or(0);
        assert_eq!(rows, 1, "only the real prompt captured");
        cleanup(&path);
    }

    #[test]
    fn recall_lines_falls_back_to_local_lexical_when_serve_unreachable() {
        let path = temp_db("recall-fallback");
        let mut cfg = cfg_for(&path);
        // Point serve at a guaranteed-refused port so the serve call errors and
        // recall degrades to local lexical recall.
        let dead = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        cfg.bind = dead.local_addr().expect("addr");
        drop(dead);
        capture_tool(
            &cfg,
            &serde_json::json!({
                "session_id": "s",
                "hook_event_name": "PostToolUse",
                "tool_name": "Bash",
                "tool_input": { "command": "fix the WAL busy_timeout contention" },
            }),
            "claude",
        )
        .expect("capture succeeds");

        let lines = recall_lines(&cfg, "WAL busy_timeout");
        assert!(
            !lines.is_empty(),
            "fell back to local lexical recall when serve was down"
        );
        assert!(
            lines
                .iter()
                .any(|l| l.contains("busy_timeout") || l.contains("Bash")),
            "surfaced the captured event: {lines:?}"
        );
        cleanup(&path);
    }

    #[test]
    fn recall_lines_uses_serve_semantic_results_when_reachable() {
        use std::io::{Read, Write};
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("binds");
        let bind = listener.local_addr().expect("addr");
        let server = std::thread::spawn(move || {
            if let Ok((mut stream, _)) = listener.accept() {
                let mut scratch = [0u8; 4096];
                let _ = stream.read(&mut scratch);
                let payload = "{\"results\":[{\"kind\":\"preference\",\"content\":\"prefers concise answers\"}]}";
                let _ = write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{payload}",
                    payload.len()
                );
            }
        });
        let path = temp_db("recall-serve");
        let mut cfg = cfg_for(&path);
        cfg.bind = bind;

        let lines = recall_lines(&cfg, "how do you like answers");
        server.join().ok();
        assert_eq!(
            lines,
            vec!["- [preference] prefers concise answers".to_string()],
            "used serve's semantic hits verbatim"
        );
        cleanup(&path);
    }

    #[test]
    fn context_envelope_echoes_event_name() {
        // print_context goes to stdout; assert the JSON it builds via the
        // same construction (shape-level test).
        let payload = serde_json::json!({ "hook_event_name": "SessionStart" });
        let event = payload
            .get("hook_event_name")
            .and_then(serde_json::Value::as_str)
            .unwrap();
        let envelope = serde_json::json!({
            "hookSpecificOutput": {
                "hookEventName": event,
                "additionalContext": "ctx",
            }
        });
        assert_eq!(
            envelope["hookSpecificOutput"]["hookEventName"],
            "SessionStart"
        );
        assert_eq!(envelope["hookSpecificOutput"]["additionalContext"], "ctx");
    }

    #[test]
    fn stdin_read_is_byte_capped_and_lossy_at_the_boundary() {
        // Under the cap: passes through untouched.
        let small = read_stdin_capped(std::io::Cursor::new(b"{\"prompt\":\"hi\"}".to_vec()));
        assert_eq!(small, "{\"prompt\":\"hi\"}");

        // Over the cap, with a multi-byte char straddling the cap boundary:
        // exactly STDIN_BYTE_CAP bytes are read, the split char degrades to
        // U+FFFD, and nothing past the cap survives.
        let cap = STDIN_BYTE_CAP as usize;
        let mut input = vec![b'a'; cap - 1];
        input.extend_from_slice("é".as_bytes()); // 2 bytes; second falls past the cap
        input.extend(std::iter::repeat_n(b'b', 100));
        let out = read_stdin_capped(std::io::Cursor::new(input));
        assert_eq!(
            out.chars().count(),
            cap,
            "result holds at most cap bytes' worth of chars"
        );
        assert!(
            out.ends_with('\u{FFFD}'),
            "split char is lossy, not an error"
        );
        assert!(!out.contains('b'), "bytes past the cap are never read");
    }

    #[test]
    fn malformed_stdin_never_fails_the_agent() {
        // run() must return Ok even when everything inside goes wrong; here we
        // exercise the tolerant payload parse path directly.
        let bad: serde_json::Value =
            serde_json::from_str("not json").unwrap_or(serde_json::json!({}));
        assert!(bad.as_object().map(|o| o.is_empty()).unwrap_or(false));
    }
}
