//! Native session-history parsers for coding agents (Claude Code, Codex,
//! OpenCode, Hermes).
//!
//! Each parser turns a foreign transcript into [`ImportUnit`]s for
//! [`crate::store::Store::import_units`]. All parsers are **lenient**: a
//! malformed record is skipped, never a reason to fail the whole import. Every
//! produced unit is truncated to [`IMPORT_TEXT_CAP`] characters *before* being
//! role-prefixed with `"[user] "`, `"[assistant] "`, or `"[tool] "`. Tool
//! *results* are imported (they carry the observations worth remembering); tool
//! call arguments and thinking blocks are skipped.

use crate::import::{IMPORT_TEXT_CAP, ImportError, ImportUnit, parse_iso8601_ms, truncate_chars};
use std::path::Path;
use std::time::Duration;

/// Build one role-prefixed unit: trim, drop empties, truncate to
/// [`IMPORT_TEXT_CAP`] (before prefixing), then prepend `"[{role}] "`.
fn unit(
    role: &str,
    text: &str,
    session: &str,
    agent: &str,
    source: &str,
    ts_ms: Option<i64>,
) -> Option<ImportUnit> {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return None;
    }
    let body = truncate_chars(trimmed, IMPORT_TEXT_CAP);
    Some(ImportUnit {
        text: format!("[{role}] {body}"),
        session_id: session.to_string(),
        agent: agent.to_string(),
        source: source.to_string(),
        ts_ms,
    })
}

/// Concatenate the `text` fields of `{type:"text"}` blocks in a content array.
fn joined_text_blocks(blocks: &[serde_json::Value]) -> String {
    blocks
        .iter()
        .filter(|block| block.get("type").and_then(serde_json::Value::as_str) == Some("text"))
        .filter_map(|block| block.get("text").and_then(serde_json::Value::as_str))
        .collect::<Vec<_>>()
        .join("\n")
}

/// A `tool_result` block's `content` is either a plain string or an array of
/// `{type:"text"}` blocks; anything else yields an empty string (skipped later).
fn claude_tool_result_text(content: &serde_json::Value) -> String {
    match content {
        serde_json::Value::String(text) => text.clone(),
        serde_json::Value::Array(blocks) => joined_text_blocks(blocks),
        _ => String::new(),
    }
}

/// Parse a Claude Code session transcript (`~/.claude/projects/**/*.jsonl`).
///
/// Only `type` `"user"`/`"assistant"` records on the main chain are kept
/// (summary/system/file-history-snapshot records and `isSidechain` branches are
/// skipped). User text becomes one `[user]` unit per record, each `tool_result`
/// block becomes its own `[tool]` unit, and assistant `text` blocks join into
/// one `[assistant]` unit; `tool_use` and `thinking` blocks are skipped.
pub fn parse_claude_session(contents: &str, fallback_session: &str) -> Vec<ImportUnit> {
    const AGENT: &str = "claude";
    const SOURCE: &str = "claude-session";
    let mut units = Vec::new();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(serde_json::Value::Object(record)) = serde_json::from_str::<serde_json::Value>(line)
        else {
            continue;
        };
        if record
            .get("isSidechain")
            .and_then(serde_json::Value::as_bool)
            == Some(true)
        {
            continue;
        }
        let record_type = record
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or("");
        if record_type != "user" && record_type != "assistant" {
            continue;
        }
        let ts_ms = record
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso8601_ms);
        let session = record
            .get("sessionId")
            .and_then(serde_json::Value::as_str)
            .filter(|id| !id.trim().is_empty())
            .unwrap_or(fallback_session);
        let Some(content) = record.get("message").and_then(|msg| msg.get("content")) else {
            continue;
        };

        if record_type == "user" {
            match content {
                serde_json::Value::String(text) => {
                    units.extend(unit("user", text, session, AGENT, SOURCE, ts_ms));
                }
                serde_json::Value::Array(blocks) => {
                    units.extend(unit(
                        "user",
                        &joined_text_blocks(blocks),
                        session,
                        AGENT,
                        SOURCE,
                        ts_ms,
                    ));
                    for block in blocks {
                        if block.get("type").and_then(serde_json::Value::as_str)
                            != Some("tool_result")
                        {
                            continue;
                        }
                        let text = block
                            .get("content")
                            .map(claude_tool_result_text)
                            .unwrap_or_default();
                        units.extend(unit("tool", &text, session, AGENT, SOURCE, ts_ms));
                    }
                }
                _ => {}
            }
        } else if let serde_json::Value::Array(blocks) = content {
            units.extend(unit(
                "assistant",
                &joined_text_blocks(blocks),
                session,
                AGENT,
                SOURCE,
                ts_ms,
            ));
        }
    }
    units
}

/// Parse a Codex CLI rollout file (`~/.codex/sessions/**/rollout-*.jsonl`).
///
/// Lines are `{timestamp, type, payload}`. A `session_meta` line supplies the
/// session id (else `fallback_session`). `response_item` payloads of type
/// `message` become `[user]`/`[assistant]` units; `function_call_output`
/// payloads become `[tool]` units. Function *calls*, event messages, turn
/// context, and unknown types are skipped.
pub fn parse_codex_rollout(contents: &str, fallback_session: &str) -> Vec<ImportUnit> {
    const AGENT: &str = "codex";
    const SOURCE: &str = "codex-session";
    let mut units = Vec::new();
    let mut session = fallback_session.to_string();
    for line in contents.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(serde_json::Value::Object(record)) = serde_json::from_str::<serde_json::Value>(line)
        else {
            continue;
        };
        let ts_ms = record
            .get("timestamp")
            .and_then(serde_json::Value::as_str)
            .and_then(parse_iso8601_ms);
        let payload = record.get("payload");
        match record.get("type").and_then(serde_json::Value::as_str) {
            Some("session_meta") => {
                if let Some(id) = payload
                    .and_then(|p| p.get("id"))
                    .and_then(serde_json::Value::as_str)
                    .filter(|id| !id.trim().is_empty())
                {
                    session = id.to_string();
                }
            }
            Some("response_item") => {
                let Some(payload) = payload else { continue };
                match payload.get("type").and_then(serde_json::Value::as_str) {
                    Some("message") => {
                        let role = match payload.get("role").and_then(serde_json::Value::as_str) {
                            Some(role @ ("user" | "assistant")) => role,
                            _ => continue,
                        };
                        let text = payload
                            .get("content")
                            .and_then(serde_json::Value::as_array)
                            .map(|entries| {
                                entries
                                    .iter()
                                    .filter(|entry| {
                                        matches!(
                                            entry.get("type").and_then(serde_json::Value::as_str),
                                            Some("input_text" | "output_text" | "text")
                                        )
                                    })
                                    .filter_map(|entry| {
                                        entry.get("text").and_then(serde_json::Value::as_str)
                                    })
                                    .collect::<Vec<_>>()
                                    .join("\n")
                            })
                            .unwrap_or_default();
                        units.extend(unit(role, &text, &session, AGENT, SOURCE, ts_ms));
                    }
                    Some("function_call_output") => {
                        let output = payload
                            .get("output")
                            .and_then(serde_json::Value::as_str)
                            .unwrap_or("");
                        units.extend(unit("tool", output, &session, AGENT, SOURCE, ts_ms));
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }
    units
}

/// Open a foreign agent database strictly read-only (no mutation of live
/// agent state, ever) with a short busy timeout so a locked database fails
/// fast with advice instead of hanging.
fn open_readonly(path: &Path) -> Result<rusqlite::Connection, ImportError> {
    let flags =
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
    let conn =
        rusqlite::Connection::open_with_flags(path, flags).map_err(|err| db_error(path, &err))?;
    conn.busy_timeout(Duration::from_millis(250))
        .map_err(|err| db_error(path, &err))?;
    Ok(conn)
}

fn db_error(path: &Path, err: &rusqlite::Error) -> ImportError {
    ImportError::Db(format!(
        "cannot read {}: {err} (close the agent or copy the database and re-run with --path)",
        path.display()
    ))
}

fn unsupported(agent: &'static str, detail: impl Into<String>) -> ImportError {
    ImportError::UnsupportedSchema {
        agent,
        detail: detail.into(),
    }
}

fn table_exists(conn: &rusqlite::Connection, table: &str) -> Result<bool, ImportError> {
    conn.query_row(
        "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1",
        rusqlite::params![table],
        |_| Ok(()),
    )
    .map(|()| true)
    .or_else(|err| match err {
        rusqlite::Error::QueryReturnedNoRows => Ok(false),
        other => Err(ImportError::Db(other.to_string())),
    })
}

/// Column names of `table` via `PRAGMA table_info` (empty if the table is missing).
fn table_columns(conn: &rusqlite::Connection, table: &str) -> Result<Vec<String>, ImportError> {
    let mut stmt = conn
        .prepare("SELECT name FROM pragma_table_info(?1)")
        .map_err(|err| ImportError::Db(err.to_string()))?;
    let names = stmt
        .query_map(rusqlite::params![table], |row| row.get::<_, String>(0))
        .map_err(|err| ImportError::Db(err.to_string()))?
        .collect::<Result<Vec<_>, _>>()
        .map_err(|err| ImportError::Db(err.to_string()))?;
    Ok(names)
}

fn require_columns(
    agent: &'static str,
    table: &str,
    columns: &[String],
    required: &[&str],
) -> Result<(), ImportError> {
    for name in required {
        if !columns.iter().any(|col| col.eq_ignore_ascii_case(name)) {
            return Err(unsupported(
                agent,
                format!("table \"{table}\" is missing column \"{name}\""),
            ));
        }
    }
    Ok(())
}

/// Lenient SQLite value → text: TEXT as-is, numbers stringified, NULL/BLOB none.
fn value_to_string(value: &rusqlite::types::Value) -> Option<String> {
    match value {
        rusqlite::types::Value::Text(text) => Some(text.clone()),
        rusqlite::types::Value::Integer(int) => Some(int.to_string()),
        rusqlite::types::Value::Real(real) => Some(real.to_string()),
        _ => None,
    }
}

/// Time heuristic shared by the database importers: values above 10^12 are
/// already unix milliseconds, smaller positive values are seconds (possibly
/// fractional) and scale by 1000.
fn value_to_unix_ms(value: &rusqlite::types::Value) -> Option<i64> {
    let raw = match value {
        rusqlite::types::Value::Integer(int) => *int as f64,
        rusqlite::types::Value::Real(real) => *real,
        rusqlite::types::Value::Text(text) => text.trim().parse::<f64>().ok()?,
        _ => return None,
    };
    if !raw.is_finite() || raw <= 0.0 {
        return None;
    }
    let ms = if raw > 1.0e12 { raw } else { raw * 1000.0 };
    Some(ms as i64)
}

/// One message being accumulated while walking the opencode `messages ⋈ parts`
/// join: text parts concatenate into a single role unit, tool results each
/// become their own `[tool]` unit.
struct OpencodeMessage {
    role: String,
    session: String,
    ts_ms: Option<i64>,
    texts: Vec<String>,
    tools: Vec<String>,
}

impl OpencodeMessage {
    /// Build a message header from the per-message columns shared by every part
    /// row. Returns `None` (dropping the whole message) when the session or
    /// role is missing/unsupported — only `user`/`assistant` turns import,
    /// matching the lenient skip contract.
    fn from_data(
        session_id: Option<&str>,
        message_data: Option<&str>,
        ts_ms: Option<i64>,
    ) -> Option<Self> {
        let session = session_id?.to_string();
        let data: serde_json::Value = serde_json::from_str(message_data?).ok()?;
        let role = match data.get("role").and_then(serde_json::Value::as_str) {
            Some(role @ ("user" | "assistant")) => role.to_string(),
            _ => return None, // unknown roles: skip the whole message
        };
        Some(Self {
            role,
            session,
            ts_ms,
            texts: Vec::new(),
            tools: Vec::new(),
        })
    }

    fn flush_into(self, units: &mut Vec<ImportUnit>) {
        const AGENT: &str = "opencode";
        const SOURCE: &str = "opencode-session";
        if !self.texts.is_empty() {
            units.extend(unit(
                &self.role,
                &self.texts.join("\n"),
                &self.session,
                AGENT,
                SOURCE,
                self.ts_ms,
            ));
        }
        for tool in self.tools {
            units.extend(unit(
                "tool",
                &tool,
                &self.session,
                AGENT,
                SOURCE,
                self.ts_ms,
            ));
        }
    }
}

/// Extract a result-ish text from an opencode tool part's `content`: a JSON
/// object yields its `text`/`output`/`result` string field, a plain (non-JSON)
/// string is used raw, anything else is skipped.
fn opencode_tool_text(content: &str) -> Option<String> {
    match serde_json::from_str::<serde_json::Value>(content) {
        Ok(serde_json::Value::Object(obj)) => ["text", "output", "result"]
            .iter()
            .find_map(|key| obj.get(*key).and_then(serde_json::Value::as_str))
            .map(str::to_string),
        Ok(serde_json::Value::String(text)) => Some(text),
        Ok(_) => None,
        Err(_) => Some(content.to_string()),
    }
}

/// A single part row normalized for [`accumulate_opencode`]: the per-message
/// identity columns plus this part's already-extracted `payload` (the prose of
/// a text part, or the output of a tool part). `kind` is the lowercased part
/// type; `payload` is `None` when the part carries nothing importable.
struct OpencodePart {
    message_id: String,
    session_id: Option<String>,
    message_data: Option<String>,
    ts_ms: Option<i64>,
    kind: String,
    payload: Option<String>,
}

/// Fold an ordered stream of part rows into import units: consecutive parts of
/// the same message accumulate (text parts concatenate into one role unit, tool
/// parts each become a `[tool]` unit) and flush when the message id changes.
/// Shared by both OpenCode schema readers; per-row gaps are silently skipped.
fn accumulate_opencode(parts: impl Iterator<Item = OpencodePart>) -> Vec<ImportUnit> {
    let mut units = Vec::new();
    let mut current_id: Option<String> = None;
    let mut pending: Option<OpencodeMessage> = None;
    for part in parts {
        if current_id.as_deref() != Some(part.message_id.as_str()) {
            if let Some(done) = pending.take() {
                done.flush_into(&mut units);
            }
            current_id = Some(part.message_id);
            pending = OpencodeMessage::from_data(
                part.session_id.as_deref(),
                part.message_data.as_deref(),
                part.ts_ms,
            );
        }
        let Some(message) = pending.as_mut() else {
            continue; // message skipped (unknown role / bad header)
        };
        let Some(payload) = part.payload else {
            continue; // part carries no importable text
        };
        if part.kind.contains("text") {
            message.texts.push(payload);
        } else if part.kind.contains("tool") {
            message.tools.push(payload);
        }
    }
    if let Some(done) = pending.take() {
        done.flush_into(&mut units);
    }
    units
}

/// The first recognized message time column, shared by both OpenCode readers.
fn opencode_time_column(
    agent: &'static str,
    message_columns: &[String],
) -> Result<&'static str, ImportError> {
    ["time_created", "created_at", "created", "time"]
        .into_iter()
        .find(|candidate| {
            message_columns
                .iter()
                .any(|col| col.eq_ignore_ascii_case(candidate))
        })
        .ok_or_else(|| unsupported(agent, "message table has no recognized time column"))
}

/// Import an OpenCode SQLite database.
///
/// Two on-disk layouts are recognized. Current OpenCode uses singular
/// `session`/`message`/`part` tables whose `part.data` JSON carries the part
/// type and text; older databases used plural `sessions`/`messages`/`parts`
/// with `type`/`content` columns on `parts`. The schema is introspected
/// defensively and an unrecognized layout returns
/// [`ImportError::UnsupportedSchema`] rather than guessing. Per-row extraction
/// failures skip the row.
pub fn read_opencode_db(path: &Path) -> Result<Vec<ImportUnit>, ImportError> {
    let conn = open_readonly(path)?;
    if table_exists(&conn, "message")? && table_exists(&conn, "part")? {
        read_opencode_current(&conn)
    } else {
        read_opencode_legacy(&conn)
    }
}

/// Run `sql`, map each row's column values through `to_part`, and fold the
/// results into import units. Centralizes the statement-prep and row-iteration
/// boilerplate the two OpenCode schema readers would otherwise duplicate.
fn query_opencode_parts(
    conn: &rusqlite::Connection,
    sql: &str,
    to_part: impl Fn(&[rusqlite::types::Value]) -> Option<OpencodePart>,
) -> Result<Vec<ImportUnit>, ImportError> {
    let mut stmt = conn
        .prepare(sql)
        .map_err(|err| ImportError::Db(err.to_string()))?;
    let columns = stmt.column_count();
    let rows = stmt
        .query_map([], |row| {
            (0..columns)
                .map(|i| row.get::<_, rusqlite::types::Value>(i))
                .collect::<rusqlite::Result<Vec<_>>>()
        })
        .map_err(|err| ImportError::Db(err.to_string()))?;
    let parts = rows.filter_map(|row| to_part(&row.ok()?));
    Ok(accumulate_opencode(parts))
}

/// Assemble one [`OpencodePart`] from the per-message column values shared by
/// both schemas, given this part's already-extracted `kind` and `payload`.
fn opencode_part(
    message_id: String,
    session_id: &rusqlite::types::Value,
    message_data: &rusqlite::types::Value,
    time: &rusqlite::types::Value,
    kind: String,
    payload: Option<String>,
) -> OpencodePart {
    OpencodePart {
        message_id,
        session_id: value_to_string(session_id),
        message_data: value_to_string(message_data),
        ts_ms: value_to_unix_ms(time),
        kind,
        payload,
    }
}

/// Current OpenCode schema: `message ⋈ part`, where each part's type and
/// payload live in the `part.data` JSON (`$.type`, `$.text`, and a tool part's
/// `$.state.output`).
fn read_opencode_current(conn: &rusqlite::Connection) -> Result<Vec<ImportUnit>, ImportError> {
    const AGENT: &str = "opencode";
    let message_columns = table_columns(conn, "message")?;
    require_columns(
        AGENT,
        "message",
        &message_columns,
        &["id", "session_id", "data"],
    )?;
    let time_column = opencode_time_column(AGENT, &message_columns)?;
    require_columns(
        AGENT,
        "part",
        &table_columns(conn, "part")?,
        &["message_id", "data"],
    )?;

    // `time_column` comes from the fixed candidate list, never from input.
    let sql = format!(
        "SELECT m.id, m.session_id, m.data, m.{time_column}, p.data
         FROM message m JOIN part p ON p.message_id = m.id
         ORDER BY m.id, p.id"
    );
    query_opencode_parts(conn, &sql, |row| {
        let [id, session, data, time, part_data] = row else {
            return None;
        };
        let message_id = value_to_string(id)?;
        let (kind, payload) = match value_to_string(part_data)
            .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
        {
            Some(part) => {
                let kind = part
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default()
                    .to_ascii_lowercase();
                let payload = opencode_part_payload(&kind, &part);
                (kind, payload)
            }
            None => (String::new(), None),
        };
        Some(opencode_part(
            message_id, session, data, time, kind, payload,
        ))
    })
}

/// Extract the importable text from a current-schema `part.data` JSON object:
/// the prose of a non-synthetic `text` part, or the completed output of a
/// `tool` part (`$.state.output`). Reasoning, step, patch and compaction parts
/// carry no conversational text and contribute nothing.
fn opencode_part_payload(kind: &str, part: &serde_json::Value) -> Option<String> {
    if kind.contains("text") {
        if part.get("synthetic").and_then(serde_json::Value::as_bool) == Some(true) {
            return None; // system-injected text, not user-authored
        }
        part.get("text")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    } else if kind.contains("tool") {
        part.get("state")
            .and_then(|state| state.get("output"))
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
    } else {
        None
    }
}

/// Legacy OpenCode schema: plural `sessions`/`messages`/`parts` tables with
/// `type`/`content` columns on `parts`.
fn read_opencode_legacy(conn: &rusqlite::Connection) -> Result<Vec<ImportUnit>, ImportError> {
    const AGENT: &str = "opencode";
    for table in ["sessions", "messages", "parts"] {
        if !table_exists(conn, table)? {
            return Err(unsupported(AGENT, format!("missing table \"{table}\"")));
        }
    }
    let message_columns = table_columns(conn, "messages")?;
    require_columns(
        AGENT,
        "messages",
        &message_columns,
        &["id", "session_id", "data"],
    )?;
    let time_column = opencode_time_column(AGENT, &message_columns)?;
    let part_columns = table_columns(conn, "parts")?;
    require_columns(
        AGENT,
        "parts",
        &part_columns,
        &["message_id", "content", "type"],
    )?;

    // `time_column` comes from the fixed candidate list, never from input.
    let sql = format!(
        "SELECT m.id, m.session_id, m.data, m.{time_column}, p.type, p.content
         FROM messages m JOIN parts p ON p.message_id = m.id
         ORDER BY m.rowid, p.rowid"
    );
    query_opencode_parts(conn, &sql, |row| {
        let [id, session, data, time, part_type, content] = row else {
            return None;
        };
        let message_id = value_to_string(id)?;
        let kind = value_to_string(part_type)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let content = value_to_string(content);
        let payload = if kind.contains("text") {
            content
        } else if kind.contains("tool") {
            content.as_deref().and_then(opencode_tool_text)
        } else {
            None
        };
        Some(opencode_part(
            message_id, session, data, time, kind, payload,
        ))
    })
}

/// Import a Hermes SQLite database: a flat `messages` table with
/// `session_id, role, content, timestamp` (REAL unix seconds).
///
/// Roles `user`/`assistant`/`tool` import with matching prefixes; other roles
/// and NULL/empty content rows are skipped.
pub fn read_hermes_db(path: &Path) -> Result<Vec<ImportUnit>, ImportError> {
    const AGENT: &str = "hermes";
    const SOURCE: &str = "hermes-session";
    let conn = open_readonly(path)?;
    if !table_exists(&conn, "messages")? {
        return Err(unsupported(AGENT, "missing table \"messages\""));
    }
    let columns = table_columns(&conn, "messages")?;
    require_columns(
        AGENT,
        "messages",
        &columns,
        &["session_id", "role", "content", "timestamp"],
    )?;

    let mut stmt = conn
        .prepare("SELECT session_id, role, content, timestamp FROM messages ORDER BY rowid")
        .map_err(|err| ImportError::Db(err.to_string()))?;
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, rusqlite::types::Value>(0)?,
                row.get::<_, rusqlite::types::Value>(1)?,
                row.get::<_, rusqlite::types::Value>(2)?,
                row.get::<_, rusqlite::types::Value>(3)?,
            ))
        })
        .map_err(|err| ImportError::Db(err.to_string()))?;

    let mut units = Vec::new();
    for row in rows {
        let Ok((session_id, role, content, timestamp)) = row else {
            continue;
        };
        let (Some(session), Some(role), Some(content)) = (
            value_to_string(&session_id),
            value_to_string(&role),
            value_to_string(&content),
        ) else {
            continue;
        };
        if !matches!(role.as_str(), "user" | "assistant" | "tool") {
            continue;
        }
        let ts_ms = value_to_unix_ms(&timestamp);
        units.extend(unit(&role, &content, &session, AGENT, SOURCE, ts_ms));
    }
    Ok(units)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "memoryd-importers-{name}-{}-{nanos}.db",
            std::process::id()
        ))
    }

    // ---- Claude Code transcripts ----

    #[test]
    fn parse_claude_extracts_user_string_and_assistant_text_blocks() {
        let doc = concat!(
            r#"{"type":"user","sessionId":"s1","message":{"role":"user","content":"fix the wal bug"}}"#,
            "\n",
            r#"{"type":"assistant","sessionId":"s1","message":{"role":"assistant","content":[{"type":"text","text":"On it."},{"type":"text","text":"Done."}]}}"#,
        );
        let units = parse_claude_session(doc, "fb");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "[user] fix the wal bug");
        assert_eq!(units[1].text, "[assistant] On it.\nDone.");
        assert!(units.iter().all(|u| u.agent == "claude"));
        assert!(units.iter().all(|u| u.source == "claude-session"));
        assert!(units.iter().all(|u| u.session_id == "s1"));
    }

    #[test]
    fn parse_claude_extracts_tool_results_as_tool_units() {
        let doc = r#"{"type":"user","message":{"content":[{"type":"tool_result","content":"exit 0"},{"type":"tool_result","content":[{"type":"text","text":"line one"},{"type":"text","text":"line two"}]},{"type":"text","text":"looks good"}]}}"#;
        let units = parse_claude_session(doc, "fb");
        assert_eq!(units.len(), 3);
        // The combined user text comes first, then each tool_result in block order.
        assert_eq!(units[0].text, "[user] looks good");
        assert_eq!(units[1].text, "[tool] exit 0");
        assert_eq!(units[2].text, "[tool] line one\nline two");
    }

    #[test]
    fn parse_claude_skips_summary_system_snapshot_sidechain_and_thinking() {
        let doc = concat!(
            r#"{"type":"summary","summary":"earlier context"}"#,
            "\n",
            r#"{"type":"system","content":"hook output"}"#,
            "\n",
            r#"{"type":"file-history-snapshot","messageId":"x"}"#,
            "\n",
            r#"{"type":"user","isSidechain":true,"message":{"content":"sidechain question"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"content":[{"type":"thinking","thinking":"private"},{"type":"tool_use","name":"Bash","input":{"command":"rm -rf /"}},{"type":"text","text":"visible"}]}}"#,
        );
        let units = parse_claude_session(doc, "fb");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "[assistant] visible");
        assert!(!units[0].text.contains("private"));
        assert!(!units[0].text.contains("rm -rf"));
    }

    #[test]
    fn parse_claude_uses_session_id_timestamp_and_fallback() {
        let doc = concat!(
            r#"{"type":"user","sessionId":"sess-a","timestamp":"2024-01-15T12:30:45.123Z","message":{"content":"with meta"}}"#,
            "\n",
            r#"{"type":"user","timestamp":"not a timestamp","message":{"content":"without meta"}}"#,
        );
        let units = parse_claude_session(doc, "fallback-session");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].session_id, "sess-a");
        assert_eq!(units[0].ts_ms, Some(1_705_321_845_123));
        assert_eq!(units[1].session_id, "fallback-session");
        assert_eq!(units[1].ts_ms, None);
    }

    #[test]
    fn parse_claude_skips_malformed_lines_without_failing() {
        let doc = concat!(
            "not json at all\n",
            "[1,2,3]\n",
            r#"{"type":"user"}"#, // no message
            "\n",
            r#"{"type":"user","message":{"content":42}}"#, // unusable content
            "\n",
            r#"{"type":"user","message":{"content":"   "}}"#, // blank after trim
            "\n",
            r#"{"type":"user","message":{"content":"survivor"}}"#,
        );
        let units = parse_claude_session(doc, "fb");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "[user] survivor");
    }

    #[test]
    fn parse_claude_truncates_and_prefixes_roles() {
        let long = "x".repeat(IMPORT_TEXT_CAP + 100);
        let doc = format!(r#"{{"type":"user","message":{{"content":"{long}"}}}}"#);
        let units = parse_claude_session(&doc, "fb");
        assert_eq!(units.len(), 1);
        // Truncation happens before prefixing: cap chars + '…', plus "[user] ".
        let expected = format!("[user] {}…", "x".repeat(IMPORT_TEXT_CAP));
        assert_eq!(units[0].text, expected);
        assert_eq!(
            units[0].text.chars().count(),
            "[user] ".chars().count() + IMPORT_TEXT_CAP + 1
        );
    }

    // ---- Codex rollouts ----

    #[test]
    fn parse_codex_extracts_messages_and_session_meta_id() {
        let doc = concat!(
            r#"{"timestamp":"2024-01-15T12:30:45Z","type":"session_meta","payload":{"id":"codex-sess-1","cwd":"/repo"}}"#,
            "\n",
            r#"{"timestamp":"2024-01-15T12:30:46Z","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"add tests"}]}}"#,
            "\n",
            r#"{"timestamp":"2024-01-15T12:30:47Z","type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"sure"},{"type":"text","text":"running now"}]}}"#,
        );
        let units = parse_codex_rollout(doc, "fb");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "[user] add tests");
        assert_eq!(units[0].session_id, "codex-sess-1");
        assert_eq!(units[0].agent, "codex");
        assert_eq!(units[0].source, "codex-session");
        assert_eq!(units[0].ts_ms, Some(1_705_321_846_000));
        assert_eq!(units[1].text, "[assistant] sure\nrunning now");

        // Without session_meta everything carries the fallback session.
        let no_meta = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"hello"}]}}"#;
        let units = parse_codex_rollout(no_meta, "fb");
        assert_eq!(units[0].session_id, "fb");
    }

    #[test]
    fn parse_codex_extracts_function_call_output_as_tool_unit() {
        let doc = r#"{"timestamp":"2024-01-15T12:30:46Z","type":"response_item","payload":{"type":"function_call_output","call_id":"c1","output":"{\"output\":\"ok\",\"exit_code\":0}"}}"#;
        let units = parse_codex_rollout(doc, "fb");
        assert_eq!(units.len(), 1);
        // JSON-looking output is imported as the raw string, not re-parsed.
        assert_eq!(units[0].text, "[tool] {\"output\":\"ok\",\"exit_code\":0}");
        assert_eq!(units[0].ts_ms, Some(1_705_321_846_000));
    }

    #[test]
    fn parse_codex_skips_function_calls_and_events() {
        let doc = concat!(
            r#"{"type":"response_item","payload":{"type":"function_call","name":"shell","arguments":"{\"cmd\":\"ls\"}"}}"#,
            "\n",
            r#"{"type":"event_msg","payload":{"type":"agent_message","message":"streamed"}}"#,
            "\n",
            r#"{"type":"turn_context","payload":{"cwd":"/repo"}}"#,
            "\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"system","content":[{"type":"text","text":"system prompt"}]}}"#,
            "\n",
            "garbage line\n",
            r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"kept"}]}}"#,
        );
        let units = parse_codex_rollout(doc, "fb");
        assert_eq!(units.len(), 1);
        assert_eq!(units[0].text, "[user] kept");
    }

    // ---- OpenCode database ----

    fn build_opencode_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE sessions (id TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE messages (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE TABLE parts (
                 id TEXT PRIMARY KEY,
                 message_id TEXT NOT NULL,
                 type TEXT NOT NULL,
                 content TEXT NOT NULL
             );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO sessions (id, title) VALUES ('oc-sess', 'demo')",
            [],
        )
        .expect("session row");
        conn.execute(
            "INSERT INTO messages (id, session_id, time_created, data)
             VALUES ('m1', 'oc-sess', 1705321845, '{\"role\":\"user\"}')",
            [],
        )
        .expect("m1");
        conn.execute(
            "INSERT INTO messages (id, session_id, time_created, data)
             VALUES ('m2', 'oc-sess', 1705321846000, '{\"role\":\"assistant\"}')",
            [],
        )
        .expect("m2");
        conn.execute(
            "INSERT INTO messages (id, session_id, time_created, data)
             VALUES ('m3', 'oc-sess', 1705321847, '{\"role\":\"system\"}')",
            [],
        )
        .expect("m3");
        for (id, message, part_type, content) in [
            ("p1", "m1", "text", "first half"),
            ("p2", "m1", "text", "second half"),
            ("p3", "m2", "text", "assistant reply"),
            ("p4", "m2", "tool-result", r#"{"text":"tool said hi"}"#),
            (
                "p5",
                "m2",
                "tool-call",
                r#"{"arguments":"only args, no result"}"#,
            ),
            ("p6", "m3", "text", "system text must not import"),
        ] {
            conn.execute(
                "INSERT INTO parts (id, message_id, type, content) VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![id, message, part_type, content],
            )
            .expect("part row");
        }
    }

    #[test]
    fn read_opencode_extracts_text_parts_with_roles() {
        let path = temp_db_path("opencode");
        build_opencode_db(&path);

        let units = read_opencode_db(&path).expect("read succeeds");
        assert_eq!(units.len(), 3, "units: {units:?}");
        assert_eq!(units[0].text, "[user] first half\nsecond half");
        assert_eq!(units[0].session_id, "oc-sess");
        assert_eq!(units[0].agent, "opencode");
        assert_eq!(units[0].source, "opencode-session");
        // Seconds-resolution times scale to ms; ms-resolution times pass through.
        assert_eq!(units[0].ts_ms, Some(1_705_321_845_000));
        assert_eq!(units[1].text, "[assistant] assistant reply");
        assert_eq!(units[1].ts_ms, Some(1_705_321_846_000));
        assert_eq!(units[2].text, "[tool] tool said hi");
        assert!(
            units.iter().all(|u| !u.text.contains("system text")),
            "non user/assistant messages are skipped"
        );
        assert!(
            units.iter().all(|u| !u.text.contains("only args")),
            "tool parts without a result-ish payload are skipped"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_opencode_reports_unsupported_schema() {
        let path = temp_db_path("opencode-bad");
        let conn = rusqlite::Connection::open(&path).expect("create db");
        conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY);")
            .expect("schema");
        drop(conn);

        let err = read_opencode_db(&path).expect_err("missing tables rejected");
        assert!(
            matches!(
                &err,
                ImportError::UnsupportedSchema { agent: "opencode", detail }
                    if detail.contains("messages")
            ),
            "got: {err}"
        );

        let _ = fs::remove_file(&path);
    }

    fn build_opencode_db_current(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE session (id TEXT PRIMARY KEY, title TEXT);
             CREATE TABLE message (
                 id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 data TEXT NOT NULL
             );
             CREATE TABLE part (
                 id TEXT PRIMARY KEY,
                 message_id TEXT NOT NULL,
                 session_id TEXT NOT NULL,
                 time_created INTEGER NOT NULL,
                 data TEXT NOT NULL
             );",
        )
        .expect("schema");
        conn.execute(
            "INSERT INTO session (id, title) VALUES ('ses_1', 'demo')",
            [],
        )
        .expect("session row");
        // role lives inside message.data JSON, not a column (current schema).
        for (id, ts, data) in [
            ("msg_1", 1_705_321_845_000_i64, r#"{"role":"user"}"#),
            ("msg_2", 1_705_321_846_000_i64, r#"{"role":"assistant"}"#),
        ] {
            conn.execute(
                "INSERT INTO message (id, session_id, time_created, data)
                 VALUES (?1, 'ses_1', ?2, ?3)",
                rusqlite::params![id, ts, data],
            )
            .expect("message row");
        }
        // part type/text/tool-output all live inside part.data JSON.
        for (id, message, data) in [
            ("prt_1", "msg_1", r#"{"type":"text","text":"first half"}"#),
            ("prt_2", "msg_1", r#"{"type":"text","text":"second half"}"#),
            (
                "prt_3",
                "msg_1",
                r#"{"type":"text","synthetic":true,"text":"injected system context"}"#,
            ),
            (
                "prt_4",
                "msg_2",
                r#"{"type":"reasoning","text":"thinking to myself"}"#,
            ),
            (
                "prt_5",
                "msg_2",
                r#"{"type":"text","text":"assistant reply"}"#,
            ),
            (
                "prt_6",
                "msg_2",
                r#"{"type":"tool","tool":"bash","state":{"status":"completed","output":"tool said hi"}}"#,
            ),
            (
                "prt_7",
                "msg_2",
                r#"{"type":"tool","tool":"bash","state":{"status":"error","error":"boom"}}"#,
            ),
            (
                "prt_8",
                "msg_2",
                r#"{"type":"step-finish","reason":"stop"}"#,
            ),
        ] {
            conn.execute(
                "INSERT INTO part (id, message_id, session_id, time_created, data)
                 VALUES (?1, ?2, 'ses_1', 0, ?3)",
                rusqlite::params![id, message, data],
            )
            .expect("part row");
        }
    }

    #[test]
    fn read_opencode_current_schema_extracts_text_and_tool_parts() {
        let path = temp_db_path("opencode-current");
        build_opencode_db_current(&path);

        // The dispatcher must detect the singular session/message/part schema.
        let units = read_opencode_db(&path).expect("read succeeds");
        assert_eq!(units.len(), 3, "units: {units:?}");
        assert_eq!(units[0].text, "[user] first half\nsecond half");
        assert_eq!(units[0].session_id, "ses_1");
        assert_eq!(units[0].agent, "opencode");
        assert_eq!(units[0].source, "opencode-session");
        // time_created is already unix ms and passes through unscaled.
        assert_eq!(units[0].ts_ms, Some(1_705_321_845_000));
        assert_eq!(units[1].text, "[assistant] assistant reply");
        assert_eq!(units[2].text, "[tool] tool said hi");
        assert!(
            units
                .iter()
                .all(|u| !u.text.contains("injected system context")),
            "synthetic text parts are skipped"
        );
        assert!(
            units.iter().all(|u| !u.text.contains("thinking to myself")),
            "reasoning parts are not imported as conversation"
        );
        assert!(
            units.iter().all(|u| !u.text.contains("boom")),
            "errored tool parts (no output) are skipped"
        );

        let _ = fs::remove_file(&path);
    }

    // ---- Hermes database ----

    fn build_hermes_db(path: &Path) {
        let conn = rusqlite::Connection::open(path).expect("create db");
        conn.execute_batch(
            "CREATE TABLE messages (
                 id INTEGER PRIMARY KEY,
                 session_id TEXT NOT NULL,
                 role TEXT NOT NULL,
                 content TEXT,
                 timestamp REAL
             );",
        )
        .expect("schema");
        for (session, role, content, ts) in [
            ("h-sess", "user", Some("hello hermes"), 1_705_321_845.5_f64),
            ("h-sess", "assistant", Some("hi back"), 1_705_321_846.0),
            ("h-sess", "tool", Some("tool output"), 1_705_321_847.0),
            ("h-sess", "system", Some("skip me"), 1_705_321_848.0),
            ("h-sess", "user", None, 1_705_321_849.0),
            ("h-sess", "user", Some("   "), 1_705_321_850.0),
            ("h-sess", "user", Some("already ms"), 1_705_321_851_000.0),
        ] {
            conn.execute(
                "INSERT INTO messages (session_id, role, content, timestamp)
                 VALUES (?1, ?2, ?3, ?4)",
                rusqlite::params![session, role, content, ts],
            )
            .expect("message row");
        }
    }

    #[test]
    fn read_hermes_extracts_roles_including_tool_and_converts_seconds() {
        let path = temp_db_path("hermes");
        build_hermes_db(&path);

        let units = read_hermes_db(&path).expect("read succeeds");
        assert_eq!(units.len(), 4, "units: {units:?}");
        assert_eq!(units[0].text, "[user] hello hermes");
        assert_eq!(units[0].ts_ms, Some(1_705_321_845_500), "REAL seconds → ms");
        assert_eq!(units[1].text, "[assistant] hi back");
        assert_eq!(units[2].text, "[tool] tool output");
        assert_eq!(units[3].text, "[user] already ms");
        assert_eq!(
            units[3].ts_ms,
            Some(1_705_321_851_000),
            "values above 10^12 are already ms"
        );
        assert!(units.iter().all(|u| u.agent == "hermes"));
        assert!(units.iter().all(|u| u.source == "hermes-session"));
        assert!(units.iter().all(|u| u.session_id == "h-sess"));

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn read_hermes_reports_unsupported_schema() {
        let path = temp_db_path("hermes-bad");
        let conn = rusqlite::Connection::open(&path).expect("create db");
        conn.execute_batch("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT);")
            .expect("schema");
        drop(conn);

        let err = read_hermes_db(&path).expect_err("missing columns rejected");
        assert!(
            matches!(
                &err,
                ImportError::UnsupportedSchema { agent: "hermes", detail }
                    if detail.contains("session_id")
            ),
            "got: {err}"
        );

        let _ = fs::remove_file(&path);
    }

    #[test]
    fn open_readonly_never_writes_source_db() {
        let path = temp_db_path("readonly");
        build_hermes_db(&path);
        let before = fs::read(&path).expect("read db bytes before");

        let units = read_hermes_db(&path).expect("full read succeeds");
        assert!(!units.is_empty());

        let after = fs::read(&path).expect("read db bytes after");
        assert_eq!(before, after, "source database bytes are untouched");
        for suffix in ["-wal", "-shm", "-journal"] {
            let side = PathBuf::from(format!("{}{suffix}", path.display()));
            assert!(!side.exists(), "read-only open must not create {side:?}");
        }

        let _ = fs::remove_file(&path);
    }
}
