//! MCP stdio facade: hand-rolled JSON-RPC 2.0 over newline-delimited stdio
//! (protocol revision 2024-11-05), exposing the local memory store as four
//! tools (`memory_remember`, `memory_recall`, `memory_stats`, `memory_graph`).
//!
//! Design constraints: zero new dependencies, no sockets (stdio only), and no
//! background workers — captures consolidate on the next `serve`/`dream` run.
//! Protocol failures map to JSON-RPC errors; tool execution failures map to
//! in-band `isError: true` results. Store error details go to stderr only.

use memoryd_core::config::Config;
use memoryd_core::store::{Store, StoreError};
use std::io::{BufRead, Write};

pub(crate) const MCP_PROTOCOL_VERSION: &str = "2024-11-05";
const MAX_MCP_LINE_BYTES: usize = 1024 * 1024;
const GRAPH_SNIPPET_CHARS: usize = 240;

/// Run the MCP server over this process's stdin/stdout until EOF. The banner
/// goes to stderr so stdout stays a pure JSON-RPC stream.
pub(crate) fn serve_stdio(cli: crate::Cli) -> Result<(), crate::CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let mut store = Store::open(&cfg.db_path)?;
    eprintln!(
        "memoryd mcp: protocol {MCP_PROTOCOL_VERSION}, db {}",
        cfg.db_path.display()
    );

    let reader = std::io::stdin().lock();
    let mut writer = std::io::stdout().lock();
    run_loop(&mut store, &cfg, reader, &mut writer)
}

/// Process one newline-delimited JSON-RPC message per line until EOF. Blank
/// lines are skipped; oversized or unparseable lines get protocol errors with
/// a null id. Store errors never kill the loop (they surface per-request).
fn run_loop(
    store: &mut Store,
    cfg: &Config,
    reader: impl BufRead,
    writer: &mut impl Write,
) -> Result<(), crate::CliError> {
    let mut initialized = false;
    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        let response = if line.len() > MAX_MCP_LINE_BYTES {
            Some(error_response(
                serde_json::Value::Null,
                -32600,
                "request line exceeds limit",
            ))
        } else {
            match serde_json::from_str::<serde_json::Value>(&line) {
                Ok(message) => dispatch(store, cfg, &mut initialized, &message),
                Err(_) => Some(error_response(
                    serde_json::Value::Null,
                    -32700,
                    "parse error",
                )),
            }
        };
        if let Some(response) = response {
            let mut bytes = serde_json::to_vec(&response)?;
            bytes.push(b'\n');
            writer.write_all(&bytes)?;
            writer.flush()?;
        }
    }
    Ok(())
}

/// Route one JSON-RPC message. Notifications (no `id`) never get replies;
/// `initialize` and `ping` are allowed before `notifications/initialized`,
/// everything else gets -32002 until the client has initialized.
fn dispatch(
    store: &mut Store,
    cfg: &Config,
    initialized: &mut bool,
    message: &serde_json::Value,
) -> Option<serde_json::Value> {
    let method = message.get("method").and_then(serde_json::Value::as_str);
    let id = message.get("id").cloned();
    let Some(method) = method else {
        return id.map(|id| error_response(id, -32600, "invalid request"));
    };
    let Some(id) = id else {
        if method == "notifications/initialized" {
            *initialized = true;
        }
        return None;
    };
    if !*initialized && !matches!(method, "initialize" | "ping") {
        return Some(error_response(id, -32002, "server not initialized"));
    }
    match method {
        "initialize" => Some(result_response(id, initialize_result())),
        "ping" => Some(result_response(id, serde_json::json!({}))),
        "tools/list" => Some(result_response(
            id,
            serde_json::json!({ "tools": tool_definitions() }),
        )),
        "tools/call" => Some(handle_tools_call(store, cfg, id, message.get("params"))),
        _ => Some(error_response(id, -32601, "method not found")),
    }
}

fn initialize_result() -> serde_json::Value {
    serde_json::json!({
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": { "tools": {} },
        "serverInfo": { "name": "memoryd", "version": env!("CARGO_PKG_VERSION") },
        "instructions": "Local memory store. Use memory_recall before answering; \
                         memory_remember to persist durable facts; memory_graph to walk \
                         associations from a memory_id returned by recall.",
    })
}

/// The single (and only) page of tool definitions for `tools/list`.
fn tool_definitions() -> serde_json::Value {
    serde_json::json!([
        {
            "name": "memory_remember",
            "description": "Persist a durable fact or note into the local memory store. \
                            The capture is acknowledged immediately and consolidated into \
                            a recallable memory on the next dream pass.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "Text to remember." },
                    "kind": { "type": "string", "default": "note" },
                    "session_id": { "type": "string", "default": "mcp" },
                    "source": { "type": "string", "default": "mcp" },
                    "tags": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["content"]
            }
        },
        {
            "name": "memory_recall",
            "description": "Search the memory store (durable memories with one-hop graph \
                            expansion, falling back to raw events). Returns ranked hits.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Search text." },
                    "k": { "type": "integer", "minimum": 1, "maximum": 50, "default": 5 },
                    "semantic": { "type": "boolean", "default": false },
                    "hops": { "type": "integer", "enum": [0, 1], "default": 1 }
                },
                "required": ["query"]
            }
        },
        {
            "name": "memory_stats",
            "description": "Row counts for every memoryd table.",
            "inputSchema": {
                "type": "object",
                "properties": {}
            }
        },
        {
            "name": "memory_graph",
            "description": "Walk the association graph one hop out from a memory_id \
                            returned by memory_recall. Neighbors come back strongest \
                            link first, with contents truncated to short snippets.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "memory_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50, "default": 16 }
                },
                "required": ["memory_id"]
            }
        }
    ])
}

/// How a tool invocation ended: a JSON payload, or an in-band execution
/// failure (`isError: true` with a safe message).
enum ToolOutcome {
    Success(serde_json::Value),
    Failure(&'static str),
}

/// Why a tool invocation never ran: bad arguments (-32602) or an internal
/// store error (-32603, details to stderr only).
enum ToolError {
    InvalidArguments(&'static str),
    Store(StoreError),
}

fn handle_tools_call(
    store: &mut Store,
    cfg: &Config,
    id: serde_json::Value,
    params: Option<&serde_json::Value>,
) -> serde_json::Value {
    let Some(params) = params.and_then(serde_json::Value::as_object) else {
        return error_response(id, -32602, "params must be an object");
    };
    let Some(name) = params.get("name").and_then(serde_json::Value::as_str) else {
        return error_response(id, -32602, "tool name is required");
    };
    let arguments = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));

    let outcome = match name {
        "memory_remember" => call_memory_remember(store, cfg, &arguments),
        "memory_recall" => call_memory_recall(store, cfg, arguments),
        "memory_stats" => call_memory_stats(store),
        "memory_graph" => call_memory_graph(store, &arguments),
        _ => return error_response(id, -32602, "unknown tool"),
    };
    match outcome {
        Ok(ToolOutcome::Success(payload)) => match serde_json::to_string(&payload) {
            Ok(text) => result_response(id, tool_result(text, false)),
            Err(err) => {
                eprintln!("memoryd mcp: tool result serialization failed: {err}");
                error_response(id, -32603, "store error")
            }
        },
        Ok(ToolOutcome::Failure(message)) => {
            result_response(id, tool_result(message.to_string(), true))
        }
        Err(ToolError::InvalidArguments(message)) => error_response(id, -32602, message),
        Err(ToolError::Store(err)) => {
            // Never leak internals to the client; the detail goes to stderr.
            eprintln!("memoryd mcp: store error: {err}");
            error_response(id, -32603, "store error")
        }
    }
}

fn call_memory_remember(
    store: &mut Store,
    cfg: &Config,
    arguments: &serde_json::Value,
) -> Result<ToolOutcome, ToolError> {
    let object = arguments
        .as_object()
        .ok_or(ToolError::InvalidArguments("arguments must be an object"))?;
    let content = object
        .get("content")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolError::InvalidArguments("content is required"))?
        .to_string();
    let kind = optional_string(object, "kind", "note")?;
    let session_id = optional_string(object, "session_id", "mcp")?;
    let source = optional_string(object, "source", "mcp")?;
    let tags = match object.get("tags") {
        None => Vec::new(),
        Some(value) => value
            .as_array()
            .ok_or(ToolError::InvalidArguments(
                "tags must be an array of strings",
            ))?
            .iter()
            .map(|tag| {
                tag.as_str()
                    .map(ToOwned::to_owned)
                    .ok_or(ToolError::InvalidArguments(
                        "tags must be an array of strings",
                    ))
            })
            .collect::<Result<Vec<_>, _>>()?,
    };

    let event = crate::remember_event(crate::RememberArgs {
        content,
        kind,
        session_id,
        source,
        tags,
    });
    match store.capture_event_with_queue_limit(event, cfg.caps.queue_depth_max) {
        Ok(ack) => Ok(ToolOutcome::Success(serde_json::json!({
            "raw_event_id": ack.raw_event_id,
            "session_id": ack.session_id,
            "enqueued_job_id": ack.enqueued_job_id,
            "pending_memory": ack.enqueued_job_id.is_some(),
            "degraded": ack.degraded,
        }))),
        Err(StoreError::InvalidCaptureField(_)) => {
            Ok(ToolOutcome::Failure("capture fields must not be empty"))
        }
        Err(err) => Err(ToolError::Store(err)),
    }
}

fn call_memory_recall(
    store: &Store,
    cfg: &Config,
    arguments: serde_json::Value,
) -> Result<ToolOutcome, ToolError> {
    let args = crate::recall_request_from_json(arguments).map_err(ToolError::InvalidArguments)?;
    match crate::recall_with_mode(store, &args, "brute-force", &cfg.providers.default_adapter) {
        Ok(result) => Ok(ToolOutcome::Success(crate::recall_response_value(&result))),
        Err(StoreError::InvalidRecallQuery) => {
            Ok(ToolOutcome::Failure("query must contain searchable text"))
        }
        Err(err) => Err(ToolError::Store(err)),
    }
}

fn call_memory_stats(store: &Store) -> Result<ToolOutcome, ToolError> {
    let stats = store.table_stats().map_err(ToolError::Store)?;
    let tables: Vec<serde_json::Value> = stats
        .iter()
        .map(|stat| serde_json::json!({ "table": stat.table, "rows": stat.rows }))
        .collect();
    Ok(ToolOutcome::Success(
        serde_json::json!({ "tables": tables }),
    ))
}

fn call_memory_graph(
    store: &Store,
    arguments: &serde_json::Value,
) -> Result<ToolOutcome, ToolError> {
    let object = arguments
        .as_object()
        .ok_or(ToolError::InvalidArguments("arguments must be an object"))?;
    let memory_id = object
        .get("memory_id")
        .and_then(serde_json::Value::as_str)
        .ok_or(ToolError::InvalidArguments("memory_id is required"))?;
    let limit = match object.get("limit") {
        None => 16,
        Some(value) => match value.as_u64() {
            Some(limit @ 1..=50) => usize::try_from(limit).map_err(|_| {
                ToolError::InvalidArguments("limit must be an integer from 1 to 50")
            })?,
            _ => {
                return Err(ToolError::InvalidArguments(
                    "limit must be an integer from 1 to 50",
                ));
            }
        },
    };

    match store.memory_neighbors(memory_id, limit) {
        Ok(Some(hood)) => {
            let neighbors: Vec<serde_json::Value> = hood
                .neighbors
                .iter()
                .map(|n| {
                    serde_json::json!({
                        "memory_id": n.memory_id,
                        "kind": n.kind,
                        "content": snippet(&n.content, GRAPH_SNIPPET_CHARS),
                        "link_type": n.link_type,
                        "link_strength": n.link_strength,
                        "last_reinforced_at": n.last_reinforced_at,
                        "lifecycle_state": n.lifecycle_state,
                    })
                })
                .collect();
            Ok(ToolOutcome::Success(serde_json::json!({
                "memory_id": hood.memory_id,
                "kind": hood.kind,
                "content": snippet(&hood.content, GRAPH_SNIPPET_CHARS),
                "neighbors": neighbors,
            })))
        }
        Ok(None) => Ok(ToolOutcome::Failure("memory not found")),
        Err(err) => Err(ToolError::Store(err)),
    }
}

fn optional_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &str,
    default: &str,
) -> Result<String, ToolError> {
    match object.get(field) {
        None => Ok(default.to_string()),
        Some(value) => value
            .as_str()
            .map(ToOwned::to_owned)
            .ok_or(ToolError::InvalidArguments(
                "optional fields must be strings",
            )),
    }
}

/// Char-boundary-safe truncation for graph payloads: contents longer than
/// `max_chars` characters are cut there with a trailing ellipsis.
fn snippet(content: &str, max_chars: usize) -> String {
    let mut chars = content.char_indices();
    match chars.nth(max_chars) {
        None => content.to_string(),
        Some((byte_index, _)) => {
            let mut out = content[..byte_index].to_string();
            out.push('…');
            out
        }
    }
}

fn tool_result(text: String, is_error: bool) -> serde_json::Value {
    serde_json::json!({
        "content": [{ "type": "text", "text": text }],
        "isError": is_error,
    })
}

fn result_response(id: serde_json::Value, result: serde_json::Value) -> serde_json::Value {
    serde_json::json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn error_response(id: serde_json::Value, code: i64, message: &str) -> serde_json::Value {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use memoryd_core::store::NewRawEvent;
    use std::fs;
    use std::path::{Path, PathBuf};
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("memoryd-{name}-{}-{nanos}.db", std::process::id()))
    }

    fn cleanup_db_files(path: &Path) {
        for suffix in ["", "-shm", "-wal"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            let _ = fs::remove_file(file);
        }
    }

    fn open_fixture(name: &str) -> (PathBuf, Config, Store) {
        let path = temp_db_path(name);
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        (path, cfg, store)
    }

    fn capture_text(store: &mut Store, session: &str, ts_ms: i64, text: &str) {
        store
            .capture_event(NewRawEvent {
                session_id: session.to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({ "text": text }),
                provenance: serde_json::json!({}),
                ts_ms,
            })
            .expect("capture succeeds");
    }

    fn request(id: i64, method: &str) -> serde_json::Value {
        serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method })
    }

    fn tools_call(id: i64, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": name, "arguments": arguments },
        })
    }

    /// Extract and parse the single text content block of a tools/call result.
    fn tool_payload(response: &serde_json::Value) -> serde_json::Value {
        let text = response["result"]["content"][0]["text"]
            .as_str()
            .expect("tool result has a text block");
        serde_json::from_str(text).expect("tool text parses as JSON")
    }

    fn table_rows(payload: &serde_json::Value, table: &str) -> i64 {
        payload["tables"]
            .as_array()
            .expect("tables array")
            .iter()
            .find(|row| row["table"] == table)
            .and_then(|row| row["rows"].as_i64())
            .unwrap_or_default()
    }

    #[test]
    fn initialize_reports_protocol_and_tools_capability() {
        let (path, cfg, mut store) = open_fixture("mcp-init");
        let mut initialized = false;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &request(1, "initialize"),
        )
        .expect("initialize gets a reply");

        assert_eq!(response["id"], 1);
        assert_eq!(response["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(response["result"]["serverInfo"]["name"], "memoryd");
        assert!(
            response["result"]["capabilities"]["tools"].is_object(),
            "tools capability advertised"
        );
        assert!(
            response["result"]["capabilities"]
                .get("resources")
                .is_none(),
            "no resources capability"
        );
        assert!(response["result"]["instructions"].is_string());
        cleanup_db_files(&path);
    }

    #[test]
    fn requests_before_initialized_get_not_initialized_error() {
        let (path, cfg, mut store) = open_fixture("mcp-preinit");
        let mut initialized = false;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &request(7, "tools/list"),
        )
        .expect("request gets a reply");

        assert_eq!(response["id"], 7);
        assert_eq!(response["error"]["code"], -32002);
        cleanup_db_files(&path);
    }

    #[test]
    fn ping_is_allowed_before_initialized() {
        let (path, cfg, mut store) = open_fixture("mcp-ping");
        let mut initialized = false;

        let response = dispatch(&mut store, &cfg, &mut initialized, &request(2, "ping"))
            .expect("ping gets a reply");

        assert_eq!(response["id"], 2);
        assert!(response["result"].is_object());
        assert!(response.get("error").is_none());
        cleanup_db_files(&path);
    }

    #[test]
    fn tools_list_returns_exactly_four_tools() {
        let (path, cfg, mut store) = open_fixture("mcp-tools-list");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &request(3, "tools/list"),
        )
        .expect("tools/list gets a reply");

        let tools = response["result"]["tools"].as_array().expect("tools array");
        let names: Vec<&str> = tools
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name"))
            .collect();
        assert_eq!(
            names,
            [
                "memory_remember",
                "memory_recall",
                "memory_stats",
                "memory_graph"
            ]
        );
        assert_eq!(
            tools[0]["inputSchema"]["required"],
            serde_json::json!(["content"])
        );
        assert_eq!(
            tools[1]["inputSchema"]["required"],
            serde_json::json!(["query"])
        );
        assert!(tools[2]["inputSchema"].get("required").is_none());
        assert_eq!(
            tools[3]["inputSchema"]["required"],
            serde_json::json!(["memory_id"])
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_remember_persists_capture() {
        let (path, cfg, mut store) = open_fixture("mcp-remember");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(
                4,
                "memory_remember",
                serde_json::json!({ "content": "Prod migrations use flyway", "kind": "rule" }),
            ),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["result"]["isError"], false);
        let payload = tool_payload(&response);
        assert_eq!(payload["raw_event_id"], 1);
        assert_eq!(payload["session_id"], "mcp");
        assert_eq!(payload["pending_memory"], true);

        let stats = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(5, "memory_stats", serde_json::json!({})),
        )
        .expect("stats reply");
        let stats = tool_payload(&stats);
        assert_eq!(table_rows(&stats, "raw_events"), 1);
        assert_eq!(table_rows(&stats, "jobs"), 1);
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_recall_returns_seeded_lexical_hit() {
        let (path, cfg, mut store) = open_fixture("mcp-recall");
        capture_text(&mut store, "s1", 1234, "WAL timeout fixed");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(
                6,
                "memory_recall",
                serde_json::json!({ "query": "wal timeout" }),
            ),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["result"]["isError"], false);
        let payload = tool_payload(&response);
        assert_eq!(payload["results"][0]["content"], "WAL timeout fixed");
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_recall_blank_query_is_in_band_tool_error() {
        let (path, cfg, mut store) = open_fixture("mcp-recall-blank");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(7, "memory_recall", serde_json::json!({ "query": "?!" })),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["result"]["isError"], true);
        assert_eq!(
            response["result"]["content"][0]["text"],
            "query must contain searchable text"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_stats_counts_tables() {
        let (path, cfg, mut store) = open_fixture("mcp-stats");
        capture_text(&mut store, "s1", 1234, "WAL timeout fixed");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(8, "memory_stats", serde_json::json!({})),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["result"]["isError"], false);
        let payload = tool_payload(&response);
        assert_eq!(table_rows(&payload, "raw_events"), 1);
        assert_eq!(table_rows(&payload, "sessions"), 1);
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_graph_walks_co_occurrence_link_end_to_end() {
        let (path, cfg, mut store) = open_fixture("mcp-graph");
        capture_text(&mut store, "s1", 1000, "wal busy timeout fix");
        capture_text(&mut store, "s1", 1001, "vacuum schedule weekly");

        // Consolidate + associate the two same-session captures into linked memories.
        let adapter = memoryd_core::adapters::AdapterKind::from_default_adapter(
            &cfg.providers.default_adapter,
        );
        let opts = memoryd_core::dream::DreamOptions {
            trigger: "manual",
            budget_usd: cfg.caps.paid_spend_cap_usd,
            max_seconds: cfg.caps.dream_wallclock_secs,
        };
        memoryd_core::dream::dream_once(&mut store, &adapter, &cfg.caps, &opts, &|| {
            crate::unix_ms_now()
        })
        .expect("dream succeeds");

        let mut initialized = true;
        let recall = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(
                9,
                "memory_recall",
                serde_json::json!({ "query": "wal", "hops": 0 }),
            ),
        )
        .expect("recall reply");
        let recall = tool_payload(&recall);
        let memory_id = recall["results"][0]["memory_id"]
            .as_str()
            .expect("recall returns a memory_id");

        let graph = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(
                10,
                "memory_graph",
                serde_json::json!({ "memory_id": memory_id }),
            ),
        )
        .expect("graph reply");
        assert_eq!(graph["result"]["isError"], false);
        let payload = tool_payload(&graph);
        assert_eq!(payload["memory_id"], memory_id);
        let neighbors = payload["neighbors"].as_array().expect("neighbors array");
        // The local adapter may add a semantic link alongside co-occurrence (one
        // row per link_type), so look the co_occurrence edge up by type.
        let co_occurrence = neighbors
            .iter()
            .find(|n| n["link_type"] == "co_occurrence")
            .expect("the same-session sibling is linked by co_occurrence");
        assert!(
            co_occurrence["content"]
                .as_str()
                .expect("neighbor content")
                .contains("vacuum"),
            "the sibling memory comes back"
        );
        assert!(
            co_occurrence["link_strength"]
                .as_f64()
                .expect("link strength")
                > 0.0
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_graph_unknown_id_is_in_band_tool_error() {
        let (path, cfg, mut store) = open_fixture("mcp-graph-unknown");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(
                11,
                "memory_graph",
                serde_json::json!({ "memory_id": "nope" }),
            ),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["result"]["isError"], true);
        assert_eq!(response["result"]["content"][0]["text"], "memory not found");
        cleanup_db_files(&path);
    }

    #[test]
    fn unknown_tool_is_invalid_params_error() {
        let (path, cfg, mut store) = open_fixture("mcp-unknown-tool");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &tools_call(12, "memory_nonesuch", serde_json::json!({})),
        )
        .expect("tools/call gets a reply");

        assert_eq!(response["error"]["code"], -32602);
        cleanup_db_files(&path);
    }

    #[test]
    fn unknown_method_is_method_not_found() {
        let (path, cfg, mut store) = open_fixture("mcp-unknown-method");
        let mut initialized = true;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &request(13, "resources/list"),
        )
        .expect("request gets a reply");

        assert_eq!(response["error"]["code"], -32601);
        cleanup_db_files(&path);
    }

    #[test]
    fn notifications_never_get_replies() {
        let (path, cfg, mut store) = open_fixture("mcp-notification");
        let mut initialized = false;

        let response = dispatch(
            &mut store,
            &cfg,
            &mut initialized,
            &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        );

        assert!(response.is_none(), "notifications are reply-free");
        assert!(initialized, "the initialized flag is set");
        cleanup_db_files(&path);
    }

    #[test]
    fn run_loop_round_trips_initialize_and_tools_list() {
        let (path, cfg, mut store) = open_fixture("mcp-run-loop");
        let input = concat!(
            r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2024-11-05","capabilities":{},"clientInfo":{"name":"test","version":"0"}}}"#,
            "\n",
            r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
            "\n",
            r#"{"jsonrpc":"2.0","id":2,"method":"tools/list"}"#,
            "\n",
        );
        let mut output = Vec::new();

        run_loop(&mut store, &cfg, std::io::Cursor::new(input), &mut output)
            .expect("run_loop succeeds");

        let text = String::from_utf8(output).expect("output is UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 2, "two requests => exactly two response lines");
        let first: serde_json::Value = serde_json::from_str(lines[0]).expect("line parses");
        let second: serde_json::Value = serde_json::from_str(lines[1]).expect("line parses");
        assert_eq!(first["id"], 1);
        assert_eq!(first["result"]["protocolVersion"], MCP_PROTOCOL_VERSION);
        assert_eq!(second["id"], 2);
        assert_eq!(
            second["result"]["tools"].as_array().expect("tools").len(),
            4
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn run_loop_replies_parse_error_to_garbage_line() {
        let (path, cfg, mut store) = open_fixture("mcp-garbage");
        let mut output = Vec::new();

        run_loop(
            &mut store,
            &cfg,
            std::io::Cursor::new("this is not json\n"),
            &mut output,
        )
        .expect("run_loop survives garbage");

        let text = String::from_utf8(output).expect("output is UTF-8");
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines.len(), 1);
        let response: serde_json::Value = serde_json::from_str(lines[0]).expect("line parses");
        assert_eq!(response["error"]["code"], -32700);
        assert_eq!(response["id"], serde_json::Value::Null);
        cleanup_db_files(&path);
    }

    #[test]
    fn snippet_truncates_on_char_boundaries() {
        assert_eq!(snippet("short", 240), "short");
        assert_eq!(snippet("abcdef", 3), "abc…");
        // Multi-byte chars: counts characters, never splits a code point.
        assert_eq!(snippet("ééééé", 3), "ééé…");
        let exact = "x".repeat(240);
        assert_eq!(
            snippet(&exact, 240),
            exact,
            "exactly max_chars is untouched"
        );
    }
}
