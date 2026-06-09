use rusqlite::{Connection, OptionalExtension, params};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 2;

pub const CANONICAL_TABLES: [&str; 13] = [
    "sessions",
    "raw_events",
    "memories",
    "memory_versions",
    "memory_links",
    "embeddings",
    "jobs",
    "dream_runs",
    "import_batches",
    "approvals",
    "profile_facts",
    "audit_log",
    "provider_usage",
];

const REDACTED: &str = "[REDACTED]";
const AUDIT_ACTOR: &str = "memoryd";
const HIGH_ENTROPY_MIN_LEN: usize = 20;
const HIGH_ENTROPY_MIN_BITS_PER_CHAR: f64 = 4.0;

pub struct Store {
    conn: Connection,
    path: PathBuf,
}

impl Store {
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let path = path.as_ref();
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            fs::create_dir_all(parent)?;
        }

        let mut conn = Connection::open(path)?;
        apply_pragmas(&conn)?;
        migrate(&mut conn)?;

        Ok(Self {
            conn,
            path: path.to_path_buf(),
        })
    }

    pub fn capture_event(&mut self, event: NewRawEvent) -> Result<CaptureAck, StoreError> {
        self.capture_event_with_queue_limit(event, usize::MAX)
    }

    pub fn capture_event_with_queue_limit(
        &mut self,
        event: NewRawEvent,
        max_active_jobs: usize,
    ) -> Result<CaptureAck, StoreError> {
        let NewRawEvent {
            session_id,
            agent,
            source,
            kind,
            payload,
            provenance,
            ts_ms,
        } = event;

        let RedactedString {
            value: session_id,
            redactions: session_redactions,
        } = redact_inline_string_with_count(&session_id);
        let RedactedString {
            value: agent,
            redactions: agent_redactions,
        } = redact_inline_string_with_count(&agent);
        let RedactedString {
            value: source,
            redactions: source_redactions,
        } = redact_inline_string_with_count(&source);
        let RedactedString {
            value: kind,
            redactions: kind_redactions,
        } = redact_inline_string_with_count(&kind);
        validate_capture_field("session_id", &session_id)?;
        validate_capture_field("agent", &agent)?;
        validate_capture_field("source", &source)?;
        validate_capture_field("kind", &kind)?;

        let RedactedJson {
            value: payload,
            redactions: payload_redactions,
        } = redact_json_value_with_count(payload);
        let RedactedJson {
            value: provenance,
            redactions: provenance_redactions,
        } = redact_json_value_with_count(provenance);
        let redactions = session_redactions
            + agent_redactions
            + source_redactions
            + kind_redactions
            + payload_redactions
            + provenance_redactions;
        let fts_content = capture_fts_content(&payload);
        let payload = serde_json::to_string(&payload)?;
        let provenance = serde_json::to_string(&provenance)?;
        let scheduled_at = unix_ms_now();

        let tx = self.conn.transaction()?;
        tx.execute(
            "INSERT INTO sessions (id, agent, started_at, event_count, status)
             VALUES (?1, ?2, ?3, 1, 'open')
             ON CONFLICT(id) DO UPDATE SET
                started_at = CASE
                    WHEN excluded.started_at < sessions.started_at THEN excluded.started_at
                    ELSE sessions.started_at
                END,
                event_count = sessions.event_count + 1",
            params![session_id.as_str(), agent.as_str(), ts_ms],
        )?;
        tx.execute(
            "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                session_id.as_str(),
                ts_ms,
                source.as_str(),
                kind.as_str(),
                payload.as_str(),
                provenance.as_str()
            ],
        )?;
        let raw_event_id = tx.last_insert_rowid();

        tx.execute(
            "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
            params![raw_event_id, fts_content.as_str()],
        )?;

        let active_jobs = active_job_count(&tx)?;
        let job_limit = i64::try_from(max_active_jobs).unwrap_or(i64::MAX);
        let enqueued_job_id = if active_jobs < job_limit {
            let job_payload = serde_json::to_string(&serde_json::json!({
                "raw_event_id": raw_event_id,
                "session_id": session_id,
                "source": source,
                "kind": kind,
            }))?;
            tx.execute(
                "INSERT INTO jobs (kind, priority, state, payload, scheduled_at)
                 VALUES ('embed', 100, 'pending', ?1, ?2)",
                params![job_payload.as_str(), scheduled_at],
            )?;
            Some(tx.last_insert_rowid())
        } else {
            None
        };
        let degraded = enqueued_job_id.is_none();
        let raw_event_ref = raw_event_id.to_string();
        let capture_detail = serde_json::to_string(&serde_json::json!({
            "session_id": session_id,
            "source": source,
            "kind": kind,
            "enqueued_job_id": enqueued_job_id,
            "degraded": degraded,
        }))?;
        insert_audit_log(
            &tx,
            AUDIT_ACTOR,
            "capture.append",
            "raw_event",
            Some(raw_event_ref.as_str()),
            Some(capture_detail.as_str()),
            scheduled_at,
        )?;
        if redactions > 0 {
            let redaction_detail = serde_json::to_string(&serde_json::json!({
                "redactions": redactions,
                "replacement": REDACTED,
                "metadata_redactions": session_redactions + agent_redactions + source_redactions + kind_redactions,
                "payload_redactions": payload_redactions,
                "provenance_redactions": provenance_redactions,
            }))?;
            insert_audit_log(
                &tx,
                AUDIT_ACTOR,
                "redaction.apply",
                "raw_event",
                Some(raw_event_ref.as_str()),
                Some(redaction_detail.as_str()),
                scheduled_at,
            )?;
        }
        tx.commit()?;

        Ok(CaptureAck {
            raw_event_id,
            session_id,
            enqueued_job_id,
            degraded,
            processed: false,
        })
    }

    pub fn doctor_report(&self) -> Result<DoctorReport, StoreError> {
        let schema_version = self.schema_version()?;
        let journal_mode = self
            .conn
            .query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        let foreign_keys = self
            .conn
            .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))?
            == 1;
        let integrity_check = self
            .conn
            .query_row("PRAGMA integrity_check", [], |row| row.get::<_, String>(0))?;

        let mut missing_tables = Vec::new();
        for table in CANONICAL_TABLES {
            if !self.table_exists(table)? {
                missing_tables.push(table.to_string());
            }
        }
        if !self.table_exists("memories_fts")? {
            missing_tables.push("memories_fts".to_string());
        }
        if !self.table_exists("raw_events_fts")? {
            missing_tables.push("raw_events_fts".to_string());
        }
        if !self.table_exists("schema_migrations")? {
            missing_tables.push("schema_migrations".to_string());
        }

        Ok(DoctorReport {
            db_path: self.path.clone(),
            schema_version,
            journal_mode,
            foreign_keys,
            integrity_check,
            missing_tables,
        })
    }

    pub fn table_stats(&self) -> Result<Vec<TableStats>, StoreError> {
        let mut stats = Vec::with_capacity(CANONICAL_TABLES.len());
        for table in CANONICAL_TABLES {
            let sql = format!("SELECT COUNT(*) FROM {table}");
            let rows = self.conn.query_row(&sql, [], |row| row.get::<_, i64>(0))?;
            stats.push(TableStats {
                table: table.to_string(),
                rows,
            });
        }
        Ok(stats)
    }

    pub fn recall_events(&self, query: &str, limit: usize) -> Result<RecallResult, StoreError> {
        let fts_query = lexical_query(query).ok_or(StoreError::InvalidRecallQuery)?;
        let limit = i64::try_from(limit.clamp(1, 50)).unwrap_or(50);
        let mut stmt = self.conn.prepare(
            "SELECT r.id, r.session_id, r.ts, r.source, r.kind, f.content,
                    bm25(raw_events_fts) AS score
             FROM raw_events_fts AS f
             JOIN raw_events AS r ON r.id = f.raw_event_id
             WHERE raw_events_fts MATCH ?1
             ORDER BY score, r.ts DESC
             LIMIT ?2",
        )?;
        let hits = stmt
            .query_map(params![fts_query, limit], |row| {
                Ok(RecallHit {
                    raw_event_id: row.get(0)?,
                    session_id: row.get(1)?,
                    ts_ms: row.get(2)?,
                    source: row.get(3)?,
                    kind: row.get(4)?,
                    content: row.get(5)?,
                    score: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(RecallResult {
            hits,
            degraded: false,
            mode: "lexical",
        })
    }

    pub fn record_auth_rejection(
        &self,
        method: &str,
        path: &str,
        peer_loopback: Option<bool>,
        authorization_header_present: bool,
        reason: &str,
    ) -> Result<(), StoreError> {
        let detail = serde_json::to_string(&serde_json::json!({
            "method": audit_http_method(method),
            "path": audit_http_path(path),
            "peer_present": peer_loopback.is_some(),
            "peer_loopback": peer_loopback,
            "authorization_header_present": authorization_header_present,
            "reason": audit_auth_reason(reason),
        }))?;
        insert_audit_log(
            &self.conn,
            AUDIT_ACTOR,
            "auth.reject",
            "http_request",
            None,
            Some(detail.as_str()),
            unix_ms_now(),
        )
    }

    pub fn schema_version(&self) -> Result<i64, StoreError> {
        self.conn
            .query_row(
                "SELECT COALESCE(MAX(version), 0) FROM schema_migrations",
                [],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }

    fn table_exists(&self, table: &str) -> Result<bool, StoreError> {
        let exists = self
            .conn
            .query_row(
                "SELECT 1 FROM sqlite_schema WHERE type = 'table' AND name = ?1 LIMIT 1",
                [table],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        Ok(exists)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct NewRawEvent {
    pub session_id: String,
    pub agent: String,
    pub source: String,
    pub kind: String,
    pub payload: serde_json::Value,
    pub provenance: serde_json::Value,
    pub ts_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureAck {
    pub raw_event_id: i64,
    pub session_id: String,
    pub enqueued_job_id: Option<i64>,
    pub degraded: bool,
    pub processed: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallResult {
    pub hits: Vec<RecallHit>,
    pub degraded: bool,
    pub mode: &'static str,
}

#[derive(Debug, Clone, PartialEq)]
pub struct RecallHit {
    pub raw_event_id: i64,
    pub session_id: String,
    pub ts_ms: i64,
    pub source: String,
    pub kind: String,
    pub content: String,
    pub score: f64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoctorReport {
    pub db_path: PathBuf,
    pub schema_version: i64,
    pub journal_mode: String,
    pub foreign_keys: bool,
    pub integrity_check: String,
    pub missing_tables: Vec<String>,
}

impl DoctorReport {
    pub fn is_ok(&self) -> bool {
        self.schema_version == SCHEMA_VERSION
            && self.journal_mode.eq_ignore_ascii_case("wal")
            && self.foreign_keys
            && self.integrity_check == "ok"
            && self.missing_tables.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TableStats {
    pub table: String,
    pub rows: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RedactedString {
    value: String,
    redactions: usize,
}

#[derive(Debug, Clone, PartialEq)]
struct RedactedJson {
    value: serde_json::Value,
    redactions: usize,
}

#[derive(Debug)]
pub enum StoreError {
    Io(std::io::Error),
    Sql(rusqlite::Error),
    Json(serde_json::Error),
    InvalidCaptureField(&'static str),
    InvalidRecallQuery,
}

impl fmt::Display for StoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(err) => write!(f, "store I/O error: {err}"),
            Self::Sql(err) => write!(f, "SQLite error: {err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::InvalidCaptureField(field) => {
                write!(f, "capture field {field} must not be empty")
            }
            Self::InvalidRecallQuery => write!(f, "recall query must contain searchable text"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<std::io::Error> for StoreError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

impl From<rusqlite::Error> for StoreError {
    fn from(err: rusqlite::Error) -> Self {
        Self::Sql(err)
    }
}

impl From<serde_json::Error> for StoreError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

fn validate_capture_field(field: &'static str, value: &str) -> Result<(), StoreError> {
    if value.trim().is_empty() {
        return Err(StoreError::InvalidCaptureField(field));
    }
    Ok(())
}

fn insert_audit_log(
    conn: &Connection,
    actor: &str,
    action: &str,
    target_type: &str,
    target_ref: Option<&str>,
    detail: Option<&str>,
    ts_ms: i64,
) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO audit_log (ts, actor, action, target_type, target_ref, detail)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![ts_ms, actor, action, target_type, target_ref, detail],
    )?;
    Ok(())
}

fn active_job_count(conn: &Connection) -> Result<i64, StoreError> {
    conn.query_row(
        "SELECT COUNT(*) FROM jobs WHERE state IN ('pending', 'deferred', 'running')",
        [],
        |row| row.get(0),
    )
    .map_err(StoreError::from)
}

fn audit_http_method(method: &str) -> &'static str {
    match method {
        "DELETE" => "DELETE",
        "GET" => "GET",
        "HEAD" => "HEAD",
        "OPTIONS" => "OPTIONS",
        "PATCH" => "PATCH",
        "POST" => "POST",
        "PUT" => "PUT",
        _ => "other",
    }
}

fn audit_http_path(path: &str) -> &'static str {
    match path.split('?').next().unwrap_or(path) {
        "/v1/capture" => "/v1/capture",
        "/v1/recall" => "/v1/recall",
        _ => "other",
    }
}

fn audit_auth_reason(reason: &str) -> &'static str {
    match reason {
        "missing_or_invalid_bearer" => "missing_or_invalid_bearer",
        "non_loopback_peer" => "non_loopback_peer",
        "unknown_peer" => "unknown_peer",
        _ => "other",
    }
}

fn redact_json_value_with_count(mut value: serde_json::Value) -> RedactedJson {
    let redactions = redact_json_value_in_place(&mut value, None);
    RedactedJson { value, redactions }
}

fn redact_json_value_in_place(value: &mut serde_json::Value, key: Option<&str>) -> usize {
    if key.is_some_and(is_sensitive_key) {
        let was_redacted = value.as_str() == Some(REDACTED);
        if !was_redacted {
            *value = serde_json::Value::String(REDACTED.to_string());
        }
        return usize::from(!was_redacted);
    }

    match value {
        serde_json::Value::Array(values) => values
            .iter_mut()
            .map(|value| redact_json_value_in_place(value, None))
            .sum(),
        serde_json::Value::Object(object) => object
            .iter_mut()
            .map(|(key, value)| redact_json_value_in_place(value, Some(key)))
            .sum(),
        serde_json::Value::String(text) => {
            let redacted = redact_inline_string_with_count(text);
            let redactions = redacted.redactions;
            if redactions > 0 {
                *text = redacted.value;
            }
            redactions
        }
        serde_json::Value::Null | serde_json::Value::Bool(_) | serde_json::Value::Number(_) => 0,
    }
}

fn is_sensitive_key(key: &str) -> bool {
    let normalized = key
        .chars()
        .filter(|ch| ch.is_ascii_alphanumeric())
        .map(|ch| ch.to_ascii_lowercase())
        .collect::<String>();

    matches!(
        normalized.as_str(),
        "authorization"
            | "auth"
            | "password"
            | "passwd"
            | "pwd"
            | "token"
            | "apikey"
            | "credential"
            | "credentials"
            | "privatekey"
    ) || normalized.ends_with("token")
        || normalized.contains("secret")
        || normalized.contains("password")
        || normalized.contains("apikey")
        || normalized.contains("credential")
        || normalized.contains("privatekey")
}

fn redact_inline_string_with_count(input: &str) -> RedactedString {
    if input.contains("-----BEGIN") && input.contains("PRIVATE KEY-----") {
        let redactions = usize::from(input != REDACTED);
        return RedactedString {
            value: REDACTED.to_string(),
            redactions,
        };
    }

    let mut spans = Vec::new();
    collect_bearer_spans(input, &mut spans);
    collect_known_secret_prefix_spans(input, &mut spans);
    collect_email_spans(input, &mut spans);
    collect_high_entropy_spans(input, &mut spans);
    apply_redaction_spans(input, spans)
}

fn collect_bearer_spans(input: &str, spans: &mut Vec<(usize, usize)>) {
    let mut offset = 0;
    while let Some(relative) = find_ascii_case_insensitive(&input[offset..], "bearer ") {
        let secret_start = offset + relative + "bearer ".len();
        let secret_end = find_secret_end(input, secret_start);
        if secret_end > secret_start {
            spans.push((secret_start, secret_end));
        }
        offset = secret_end.max(secret_start + 1);
    }
}

fn collect_known_secret_prefix_spans(input: &str, spans: &mut Vec<(usize, usize)>) {
    for prefix in [
        "ghp_",
        "gho_",
        "ghu_",
        "ghs_",
        "ghr_",
        "github_pat_",
        "sk_live_",
        "sk_test_",
        "sk-",
        "AKIA",
        "ASIA",
    ] {
        let mut offset = 0;
        while let Some(relative) = input[offset..].find(prefix) {
            let start = offset + relative;
            let end = find_secret_end(input, start);
            let min_len = if matches!(prefix, "AKIA" | "ASIA") {
                20
            } else {
                prefix.len() + 8
            };
            if end.saturating_sub(start) >= min_len {
                spans.push((start, end));
            }
            offset = end.max(start + prefix.len());
        }
    }
}

fn collect_email_spans(input: &str, spans: &mut Vec<(usize, usize)>) {
    for (at, ch) in input.char_indices() {
        if ch != '@' {
            continue;
        }

        let start = find_email_start(input, at);
        let end = find_email_end(input, at + ch.len_utf8());
        if start < at && end > at + ch.len_utf8() && looks_like_email(&input[start..end]) {
            spans.push((start, end));
        }
    }
}

fn collect_high_entropy_spans(input: &str, spans: &mut Vec<(usize, usize)>) {
    let mut start = None;
    for (index, ch) in input.char_indices() {
        if is_token_char(ch) {
            start.get_or_insert(index);
            continue;
        }

        if let Some(token_start) = start.take() {
            push_high_entropy_span(input, token_start, index, spans);
        }
    }

    if let Some(token_start) = start {
        push_high_entropy_span(input, token_start, input.len(), spans);
    }
}

fn push_high_entropy_span(input: &str, start: usize, end: usize, spans: &mut Vec<(usize, usize)>) {
    let candidate = &input[start..end];
    if candidate.len() >= HIGH_ENTROPY_MIN_LEN
        && candidate.bytes().any(|byte| byte.is_ascii_alphabetic())
        && candidate.bytes().any(|byte| byte.is_ascii_digit())
        && shannon_entropy(candidate) >= HIGH_ENTROPY_MIN_BITS_PER_CHAR
    {
        spans.push((start, end));
    }
}

fn apply_redaction_spans(input: &str, mut spans: Vec<(usize, usize)>) -> RedactedString {
    if spans.is_empty() {
        return RedactedString {
            value: input.to_string(),
            redactions: 0,
        };
    }

    spans.sort_unstable_by_key(|span| span.0);
    let mut output = String::with_capacity(input.len());
    let mut cursor = 0;
    let mut redactions = 0;
    for (start, end) in spans {
        if start < cursor || start >= end || end > input.len() {
            continue;
        }
        output.push_str(&input[cursor..start]);
        output.push_str(REDACTED);
        cursor = end;
        redactions += 1;
    }
    output.push_str(&input[cursor..]);
    RedactedString {
        value: output,
        redactions,
    }
}

fn find_ascii_case_insensitive(haystack: &str, needle: &str) -> Option<usize> {
    haystack
        .as_bytes()
        .windows(needle.len())
        .position(|window| window.eq_ignore_ascii_case(needle.as_bytes()))
}

fn find_secret_end(input: &str, start: usize) -> usize {
    input[start..]
        .char_indices()
        .find_map(|(relative, ch)| is_secret_terminator(ch).then_some(start + relative))
        .unwrap_or(input.len())
}

fn find_email_start(input: &str, at: usize) -> usize {
    input[..at]
        .char_indices()
        .filter_map(|(index, ch)| is_email_boundary(ch).then_some(index + ch.len_utf8()))
        .next_back()
        .unwrap_or(0)
}

fn find_email_end(input: &str, after_at: usize) -> usize {
    input[after_at..]
        .char_indices()
        .find_map(|(relative, ch)| is_email_boundary(ch).then_some(after_at + relative))
        .unwrap_or(input.len())
}

fn looks_like_email(candidate: &str) -> bool {
    let Some((local, domain)) = candidate.split_once('@') else {
        return false;
    };
    !local.is_empty()
        && domain.contains('.')
        && domain.len() >= 3
        && candidate
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '%' | '+' | '-' | '@'))
}

fn shannon_entropy(candidate: &str) -> f64 {
    let mut counts = [0usize; 128];
    let mut len = 0usize;
    for byte in candidate.bytes().filter(|byte| byte.is_ascii()) {
        counts[usize::from(byte)] += 1;
        len += 1;
    }
    if len == 0 {
        return 0.0;
    }

    counts
        .into_iter()
        .filter(|count| *count > 0)
        .map(|count| {
            let probability = count as f64 / len as f64;
            -probability * probability.log2()
        })
        .sum()
}

fn is_secret_terminator(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            ',' | ';' | ':' | ')' | '(' | '[' | ']' | '{' | '}' | '<' | '>' | '"' | '\'' | '`'
        )
}

fn is_email_boundary(ch: char) -> bool {
    ch.is_whitespace()
        || matches!(
            ch,
            '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}' | '"' | '\'' | '`' | ',' | ';' | ':'
        )
}

fn is_token_char(ch: char) -> bool {
    ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-' | '.' | '=' | '+')
}

fn capture_fts_content(payload: &serde_json::Value) -> String {
    payload
        .as_object()
        .and_then(|object| object.get("text"))
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| payload.to_string())
}

fn lexical_query(query: &str) -> Option<String> {
    let tokens = query
        .split(|ch: char| !ch.is_alphanumeric() && ch != '_')
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\""))
        .take(20)
        .collect::<Vec<_>>();
    if tokens.is_empty() {
        return None;
    }
    Some(tokens.join(" OR "))
}

fn unix_ms_now() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

fn apply_pragmas(conn: &Connection) -> Result<(), StoreError> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA foreign_keys = ON;
         PRAGMA busy_timeout = 5000;",
    )?;
    Ok(())
}

fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let tx = conn.transaction()?;
    tx.execute_batch(
        "CREATE TABLE IF NOT EXISTS schema_migrations (
            version INTEGER PRIMARY KEY,
            name TEXT NOT NULL,
            applied_at INTEGER NOT NULL
        ) STRICT;",
    )?;

    let applied_0001 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [1],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0001 {
        tx.execute_batch(MIGRATION_0001)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (1, "0001_foundation"),
        )?;
    }

    let applied_0002 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [2],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0002 {
        tx.execute_batch(MIGRATION_0002)?;
        let existing_events = {
            let mut stmt = tx.prepare("SELECT id, payload FROM raw_events ORDER BY id")?;
            stmt.query_map([], |row| {
                Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        for (raw_event_id, payload) in existing_events {
            let payload = serde_json::from_str::<serde_json::Value>(&payload)?;
            let fts_content = capture_fts_content(&payload);
            tx.execute(
                "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
                params![raw_event_id, fts_content],
            )?;
        }
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (2, "0002_raw_event_fts"),
        )?;
    }

    tx.commit()?;
    Ok(())
}

const MIGRATION_0001: &str = r#"
CREATE TABLE sessions (
    id TEXT PRIMARY KEY,
    agent TEXT NOT NULL,
    started_at INTEGER NOT NULL,
    ended_at INTEGER,
    summary TEXT,
    event_count INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'open'
        CHECK (status IN ('open', 'closed', 'consolidated'))
) STRICT;
CREATE INDEX sessions_agent_started ON sessions(agent, started_at DESC);
CREATE INDEX sessions_open ON sessions(status) WHERE status = 'open';

CREATE TABLE raw_events (
    id INTEGER PRIMARY KEY,
    session_id TEXT NOT NULL REFERENCES sessions(id) ON DELETE CASCADE,
    ts INTEGER NOT NULL,
    source TEXT NOT NULL,
    kind TEXT NOT NULL,
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    provenance TEXT NOT NULL CHECK (json_valid(provenance)),
    processed_at INTEGER
) STRICT;
CREATE INDEX raw_events_unprocessed ON raw_events(id) WHERE processed_at IS NULL;
CREATE INDEX raw_events_session_ts ON raw_events(session_id, ts);

CREATE TABLE memories (
    id TEXT PRIMARY KEY,
    current_version_id TEXT,
    kind TEXT NOT NULL,
    content TEXT NOT NULL,
    lifecycle_state TEXT NOT NULL DEFAULT 'captured'
        CHECK (lifecycle_state IN (
            'captured', 'triaged', 'embedded', 'consolidated', 'active',
            'associated', 'decaying', 'dormant', 'archived', 'purged'
        )),
    relevance_score REAL NOT NULL DEFAULT 0.0,
    last_accessed_at INTEGER,
    access_count INTEGER NOT NULL DEFAULT 0,
    decay_at INTEGER,
    created_at INTEGER NOT NULL,
    FOREIGN KEY (current_version_id) REFERENCES memory_versions(id)
        DEFERRABLE INITIALLY DEFERRED
) STRICT;
CREATE INDEX memories_state_decay ON memories(lifecycle_state, decay_at);
CREATE INDEX memories_decay_due ON memories(decay_at)
    WHERE lifecycle_state IN ('active', 'associated', 'decaying', 'dormant');
CREATE INDEX memories_active_recent ON memories(last_accessed_at DESC)
    WHERE lifecycle_state IN ('active', 'associated');

CREATE VIRTUAL TABLE memories_fts USING fts5(
    memory_id UNINDEXED,
    content,
    tokenize = 'unicode61 remove_diacritics 2'
);

CREATE TABLE jobs (
    id INTEGER PRIMARY KEY,
    kind TEXT NOT NULL CHECK (kind IN (
        'triage', 'embed', 'consolidate', 'decay', 'associate', 'dedup',
        'extract_profile', 'import', 'access_bump', 'cleanup'
    )),
    priority INTEGER NOT NULL DEFAULT 100,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'running', 'done', 'failed', 'dead', 'deferred')),
    payload TEXT NOT NULL CHECK (json_valid(payload)),
    attempts INTEGER NOT NULL DEFAULT 0,
    scheduled_at INTEGER NOT NULL,
    started_at INTEGER,
    finished_at INTEGER,
    last_error TEXT
) STRICT;
CREATE INDEX jobs_ready ON jobs(priority, scheduled_at, id)
    WHERE state IN ('pending', 'deferred');
CREATE INDEX jobs_state_kind ON jobs(state, kind);

CREATE TABLE memory_versions (
    id TEXT PRIMARY KEY,
    memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    version_no INTEGER NOT NULL,
    content TEXT NOT NULL,
    reason TEXT NOT NULL,
    created_by_job INTEGER REFERENCES jobs(id) ON DELETE SET NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (memory_id, version_no)
) STRICT;

CREATE TABLE memory_links (
    id TEXT PRIMARY KEY,
    src_memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    dst_memory_id TEXT NOT NULL REFERENCES memories(id) ON DELETE CASCADE,
    link_type TEXT NOT NULL CHECK (link_type IN (
        'semantic', 'causal', 'temporal', 'co_occurrence', 'dedup',
        'supersedes', 'contradicts'
    )),
    weight REAL NOT NULL DEFAULT 0.0,
    last_reinforced_at INTEGER NOT NULL,
    CHECK (src_memory_id <> dst_memory_id),
    UNIQUE (src_memory_id, dst_memory_id, link_type)
) STRICT;
CREATE INDEX memory_links_src ON memory_links(src_memory_id, weight DESC);
CREATE INDEX memory_links_dst ON memory_links(dst_memory_id);
CREATE INDEX memory_links_weak ON memory_links(last_reinforced_at) WHERE weight < 0.1;

CREATE TABLE embeddings (
    id TEXT PRIMARY KEY,
    owner_type TEXT NOT NULL CHECK (owner_type IN ('memory', 'query', 'raw_event')),
    owner_id TEXT NOT NULL,
    model_id TEXT NOT NULL,
    dim INTEGER NOT NULL,
    vector BLOB NOT NULL,
    created_at INTEGER NOT NULL,
    UNIQUE (owner_type, owner_id, model_id)
) STRICT;
CREATE INDEX embeddings_owner_model ON embeddings(owner_type, model_id, owner_id);

CREATE TABLE dream_runs (
    id TEXT PRIMARY KEY,
    trigger TEXT NOT NULL CHECK (trigger IN ('scheduled', 'idle', 'queue_depth', 'manual')),
    started_at INTEGER NOT NULL,
    finished_at INTEGER,
    jobs_run INTEGER NOT NULL DEFAULT 0,
    memories_touched INTEGER NOT NULL DEFAULT 0,
    tokens_used INTEGER NOT NULL DEFAULT 0,
    status TEXT NOT NULL DEFAULT 'running'
        CHECK (status IN ('running', 'completed', 'partial', 'degraded', 'budget_capped', 'aborted', 'failed'))
) STRICT;
CREATE INDEX dream_runs_started ON dream_runs(started_at DESC);

CREATE TABLE import_batches (
    id TEXT PRIMARY KEY,
    source TEXT NOT NULL,
    path_or_uri TEXT NOT NULL,
    total INTEGER NOT NULL DEFAULT 0,
    processed INTEGER NOT NULL DEFAULT 0,
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'scanning', 'staging', 'paused', 'consolidating', 'completed', 'failed')),
    cursor TEXT,
    skipped INTEGER NOT NULL DEFAULT 0,
    content_root_sha BLOB,
    started_at INTEGER,
    finished_at INTEGER,
    UNIQUE (source, path_or_uri)
) STRICT;

CREATE TABLE approvals (
    id TEXT PRIMARY KEY,
    target_type TEXT NOT NULL CHECK (target_type IN ('profile_fact', 'memory_cleanup', 'memory_purge', 'other')),
    target_ref TEXT,
    proposed_change TEXT NOT NULL CHECK (json_valid(proposed_change)),
    state TEXT NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'approved', 'rejected', 'expired')),
    requested_at INTEGER NOT NULL,
    decided_at INTEGER
) STRICT;
CREATE INDEX approvals_pending ON approvals(requested_at) WHERE state = 'pending';

CREATE TABLE profile_facts (
    id TEXT PRIMARY KEY,
    fact_key TEXT NOT NULL,
    fact_value TEXT NOT NULL,
    confidence REAL NOT NULL DEFAULT 0.0,
    source_memory_id TEXT REFERENCES memories(id) ON DELETE SET NULL,
    approval_id TEXT NOT NULL REFERENCES approvals(id),
    state TEXT NOT NULL DEFAULT 'active'
        CHECK (state IN ('active', 'superseded', 'retracted')),
    created_at INTEGER NOT NULL
) STRICT;
CREATE UNIQUE INDEX profile_facts_active_key ON profile_facts(fact_key)
    WHERE state = 'active';

CREATE TABLE audit_log (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    actor TEXT NOT NULL,
    action TEXT NOT NULL,
    target_type TEXT NOT NULL,
    target_ref TEXT,
    detail TEXT CHECK (detail IS NULL OR json_valid(detail))
) STRICT;
CREATE INDEX audit_log_ts ON audit_log(ts);
CREATE INDEX audit_log_target ON audit_log(target_type, target_ref, ts);
CREATE TRIGGER audit_log_no_update BEFORE UPDATE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;
CREATE TRIGGER audit_log_no_delete BEFORE DELETE ON audit_log
BEGIN
    SELECT RAISE(ABORT, 'audit_log is append-only');
END;

CREATE TABLE provider_usage (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    adapter TEXT NOT NULL CHECK (adapter IN ('openai_compat', 'ollama', 'opencode', 'null')),
    model_id TEXT NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('embed', 'complete', 'embed:dry', 'complete:dry')),
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    est_cost REAL NOT NULL DEFAULT 0.0,
    job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL
) STRICT;
CREATE INDEX provider_usage_ts_adapter ON provider_usage(ts, adapter);
"#;

const MIGRATION_0002: &str = r#"
CREATE VIRTUAL TABLE raw_events_fts USING fts5(
    raw_event_id UNINDEXED,
    content,
    tokenize = 'unicode61 remove_diacritics 2'
);
"#;

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

    #[test]
    fn open_creates_schema_and_wal_pragmas() {
        let path = temp_db_path("doctor");
        let store = Store::open(&path).expect("store opens");
        let report = store.doctor_report().expect("doctor report");

        assert!(report.is_ok(), "doctor report was {report:?}");
        assert_eq!(report.schema_version, SCHEMA_VERSION);
        assert!(report.missing_tables.is_empty());

        cleanup_db_files(&path);
    }

    #[test]
    fn migration_is_idempotent() {
        let path = temp_db_path("idempotent");
        Store::open(&path).expect("first open");
        let store = Store::open(&path).expect("second open");

        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn migration_0002_backfills_existing_raw_events_for_recall() {
        let path = temp_db_path("migration-backfill");
        {
            let conn = rusqlite::Connection::open(&path).expect("connection opens");
            apply_pragmas(&conn).expect("pragmas apply");
            conn.execute_batch(
                "CREATE TABLE IF NOT EXISTS schema_migrations (
                    version INTEGER PRIMARY KEY,
                    name TEXT NOT NULL,
                    applied_at INTEGER NOT NULL
                ) STRICT;",
            )
            .expect("schema migration table exists");
            conn.execute_batch(MIGRATION_0001)
                .expect("v1 schema applies");
            conn.execute(
                "INSERT INTO schema_migrations (version, name, applied_at) VALUES (?1, ?2, ?3)",
                params![1, "0001_foundation", 1000_i64],
            )
            .expect("v1 migration recorded");
            conn.execute(
                "INSERT INTO sessions (id, agent, started_at) VALUES (?1, ?2, ?3)",
                params!["session-1", "claude", 1000_i64],
            )
            .expect("session inserted");
            conn.execute(
                "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    "session-1",
                    1000_i64,
                    "tool_result",
                    "observation",
                    r#"{"text":"Legacy WAL backfilled"}"#,
                    "{}"
                ],
            )
            .expect("raw event inserted");
        }

        let store = Store::open(&path).expect("store migrates");
        assert_eq!(
            store.schema_version().expect("schema version"),
            SCHEMA_VERSION
        );
        let result = store
            .recall_events("legacy wal", 5)
            .expect("recall succeeds");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].content, "Legacy WAL backfilled");

        cleanup_db_files(&path);
    }

    #[test]
    fn stats_include_all_canonical_tables() {
        let path = temp_db_path("stats");
        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("table stats");

        assert_eq!(stats.len(), CANONICAL_TABLES.len());
        assert!(stats.iter().all(|stat| stat.rows == 0));
        assert_eq!(stats[0].table, "sessions");

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_appends_session_event_and_embed_job() {
        let path = temp_db_path("capture");
        let mut store = Store::open(&path).expect("store opens");

        let ack = store
            .capture_event(NewRawEvent {
                session_id: "session-1".to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({"text": "busy_timeout fixed WAL contention"}),
                provenance: serde_json::json!({"tags": ["db", "wal"]}),
                ts_ms: 1234,
            })
            .expect("capture succeeds");

        assert!(ack.raw_event_id > 0);
        assert_eq!(ack.session_id, "session-1");
        let enqueued_job_id = ack.enqueued_job_id.expect("job queued");
        assert!(enqueued_job_id > 0);
        assert!(!ack.degraded);
        assert!(!ack.processed);

        let session_count = store
            .conn
            .query_row(
                "SELECT event_count FROM sessions WHERE id = 'session-1'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("session row exists");
        assert_eq!(session_count, 1);

        let (source, kind, payload): (String, String, String) = store
            .conn
            .query_row(
                "SELECT source, kind, payload FROM raw_events WHERE id = ?1",
                [ack.raw_event_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("raw event row exists");
        assert_eq!(source, "tool_result");
        assert_eq!(kind, "observation");
        assert_eq!(payload, r#"{"text":"busy_timeout fixed WAL contention"}"#);

        let (job_kind, job_state, job_payload): (String, String, String) = store
            .conn
            .query_row(
                "SELECT kind, state, payload FROM jobs WHERE id = ?1",
                [enqueued_job_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("job row exists");
        let job_payload: serde_json::Value =
            serde_json::from_str(&job_payload).expect("job payload is json");
        assert_eq!(job_kind, "embed");
        assert_eq!(job_state, "pending");
        assert_eq!(job_payload["raw_event_id"], ack.raw_event_id);

        let provider_rows = store
            .conn
            .query_row("SELECT COUNT(*) FROM provider_usage", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("provider usage count");
        assert_eq!(provider_rows, 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_degrades_without_enqueue_when_job_queue_is_at_limit() {
        let path = temp_db_path("capture-queue-limit");
        let mut store = Store::open(&path).expect("store opens");

        let ack = store
            .capture_event_with_queue_limit(test_event("session-1", 1234), 0)
            .expect("capture succeeds in degraded mode");

        assert!(ack.raw_event_id > 0);
        assert_eq!(ack.enqueued_job_id, None);
        assert!(ack.degraded);
        assert!(!ack.processed);

        let raw_events = store
            .conn
            .query_row("SELECT COUNT(*) FROM raw_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("raw event count");
        let sessions = store
            .conn
            .query_row("SELECT COUNT(*) FROM sessions", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("session count");
        let jobs = store
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))
            .expect("job count");
        assert_eq!(raw_events, 1);
        assert_eq!(sessions, 1);
        assert_eq!(jobs, 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_reuses_session_and_increments_event_count() {
        let path = temp_db_path("capture-session-upsert");
        let mut store = Store::open(&path).expect("store opens");

        store
            .capture_event(test_event("session-1", 2000))
            .expect("first capture succeeds");
        store
            .capture_event(test_event("session-1", 1000))
            .expect("second capture succeeds");

        let (started_at, event_count): (i64, i64) = store
            .conn
            .query_row(
                "SELECT started_at, event_count FROM sessions WHERE id = 'session-1'",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("session row exists");
        assert_eq!(started_at, 1000);
        assert_eq!(event_count, 2);

        let event_rows = store
            .conn
            .query_row("SELECT COUNT(*) FROM raw_events", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("raw event count");
        let job_rows = store
            .conn
            .query_row("SELECT COUNT(*) FROM jobs", [], |row| row.get::<_, i64>(0))
            .expect("job count");
        assert_eq!(event_rows, 2);
        assert_eq!(job_rows, 2);

        cleanup_db_files(&path);
    }

    #[test]
    #[ignore = "performance evidence fixture; run explicitly on an idle host"]
    fn capture_100_sequential_inserts_p95_stays_under_m1_target() {
        let path = temp_db_path("capture-latency");
        let mut store = Store::open(&path).expect("store opens");
        let mut durations = Vec::with_capacity(100);

        for index in 0..100 {
            let started = Instant::now();
            store
                .capture_event(test_event("session-1", 1_000 + index))
                .expect("capture succeeds");
            durations.push(started.elapsed());
        }

        durations.sort_unstable();
        let p95 = durations[94];
        eprintln!("capture_100_seq_p95={p95:?}");
        assert!(
            p95 < Duration::from_millis(8),
            "capture p95 {p95:?} exceeded M1 target"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_redacts_payload_provenance_and_fts_before_persistence() {
        let path = temp_db_path("capture-redacts-before-persistence");
        let mut store = Store::open(&path).expect("store opens");
        let api_key = "structuredapikeyvalue";
        let bearer = "leakycredentialvalue";
        let password = "correct-horse-battery-staple";
        let email = "ops@example.test";
        let provenance_token = "provenancesecretvalue";

        store
            .capture_event(NewRawEvent {
                session_id: "session-1".to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({
                    "text": format!("Deploy with Authorization: Bearer {bearer}; contact {email}"),
                    "api_key": api_key,
                    "nested": {"password": password},
                }),
                provenance: serde_json::json!({"token": provenance_token}),
                ts_ms: 1234,
            })
            .expect("capture succeeds");

        let (payload, provenance): (String, String) = store
            .conn
            .query_row(
                "SELECT payload, provenance FROM raw_events WHERE id = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("raw event row exists");
        let fts_content: String = store
            .conn
            .query_row(
                "SELECT content FROM raw_events_fts WHERE raw_event_id = 1",
                [],
                |row| row.get(0),
            )
            .expect("fts row exists");

        for stored in [&payload, &provenance, &fts_content] {
            for secret in [api_key, bearer, password, email, provenance_token] {
                assert!(!stored.contains(secret), "stored value leaked {secret}");
            }
        }

        let payload: serde_json::Value = serde_json::from_str(&payload).expect("payload is JSON");
        let provenance: serde_json::Value =
            serde_json::from_str(&provenance).expect("provenance is JSON");
        assert_eq!(payload["api_key"], "[REDACTED]");
        assert_eq!(payload["nested"]["password"], "[REDACTED]");
        assert_eq!(provenance["token"], "[REDACTED]");
        assert_eq!(
            payload["text"],
            "Deploy with Authorization: Bearer [REDACTED]; contact [REDACTED]"
        );
        assert_eq!(
            fts_content,
            "Deploy with Authorization: Bearer [REDACTED]; contact [REDACTED]"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_records_audit_rows_for_capture_and_redaction() {
        let path = temp_db_path("capture-audit");
        let mut store = Store::open(&path).expect("store opens");
        let secret = "leakycredentialvalue";

        let ack = store
            .capture_event(NewRawEvent {
                session_id: "session-1".to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({
                    "text": format!("Authorization: Bearer {secret}"),
                }),
                provenance: serde_json::json!({}),
                ts_ms: 1234,
            })
            .expect("capture succeeds");

        let audit_rows = store
            .conn
            .prepare(
                "SELECT action, target_type, target_ref, detail
                 FROM audit_log
                 ORDER BY id",
            )
            .expect("audit query prepares")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })
            .expect("audit query runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("audit rows collect");

        let raw_event_id = ack.raw_event_id.to_string();
        assert_eq!(audit_rows.len(), 2);
        assert_eq!(audit_rows[0].0, "capture.append");
        assert_eq!(audit_rows[0].1, "raw_event");
        assert_eq!(audit_rows[0].2.as_deref(), Some(raw_event_id.as_str()));
        assert_eq!(audit_rows[1].0, "redaction.apply");
        assert_eq!(audit_rows[1].1, "raw_event");
        assert_eq!(audit_rows[1].2.as_deref(), Some(raw_event_id.as_str()));

        for (_, _, _, detail) in &audit_rows {
            let detail = detail.as_deref().expect("audit detail exists");
            assert!(!detail.contains(secret));
        }
        let redaction_detail: serde_json::Value =
            serde_json::from_str(audit_rows[1].3.as_deref().expect("redaction detail exists"))
                .expect("redaction detail is json");
        assert_eq!(redaction_detail["redactions"], 1);
        assert_eq!(redaction_detail["replacement"], REDACTED);

        cleanup_db_files(&path);
    }

    #[test]
    fn auth_rejection_audit_row_uses_safe_request_classes() {
        let path = temp_db_path("auth-rejection-audit");
        let store = Store::open(&path).expect("store opens");
        let secret = "presentedsupersecretvalue";

        store
            .record_auth_rejection(
                secret,
                &format!("/v1/capture?token={secret}"),
                Some(false),
                true,
                &format!("bad-{secret}"),
            )
            .expect("auth rejection audit succeeds");

        let (action, target_type, target_ref, detail): (String, String, Option<String>, String) =
            store
                .conn
                .query_row(
                    "SELECT action, target_type, target_ref, detail FROM audit_log",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
                )
                .expect("audit row exists");
        assert_eq!(action, "auth.reject");
        assert_eq!(target_type, "http_request");
        assert_eq!(target_ref, None);
        assert!(!detail.contains(secret));

        let detail: serde_json::Value = serde_json::from_str(&detail).expect("detail is json");
        assert_eq!(detail["method"], "other");
        assert_eq!(detail["path"], "/v1/capture");
        assert_eq!(detail["peer_present"], true);
        assert_eq!(detail["peer_loopback"], false);
        assert_eq!(detail["authorization_header_present"], true);
        assert_eq!(detail["reason"], "other");

        cleanup_db_files(&path);
    }

    #[test]
    fn capture_redacts_metadata_fields_before_persistence() {
        let path = temp_db_path("capture-redacts-metadata");
        let mut store = Store::open(&path).expect("store opens");
        let session_secret = "sessioncredentialvalue";
        let agent_secret = "agentcredentialvalue";
        let source_secret = "sourcecredentialvalue";
        let kind_secret = "kindcredentialvalue";

        let ack = store
            .capture_event(NewRawEvent {
                session_id: format!("Bearer {session_secret}"),
                agent: format!("Authorization: Bearer {agent_secret}"),
                source: format!("source bearer {source_secret}"),
                kind: format!("kind bearer {kind_secret}"),
                payload: serde_json::json!({"text": "metadata redaction"}),
                provenance: serde_json::json!({}),
                ts_ms: 1234,
            })
            .expect("capture succeeds");

        let (session_id, agent, source, kind, job_payload): (
            String,
            String,
            String,
            String,
            String,
        ) = store
            .conn
            .query_row(
                "SELECT s.id, s.agent, r.source, r.kind, j.payload
                 FROM sessions AS s
                 JOIN raw_events AS r ON r.session_id = s.id
                 JOIN jobs AS j ON j.id = ?1",
                [ack.enqueued_job_id.expect("job queued")],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                    ))
                },
            )
            .expect("stored rows exist");

        assert_eq!(ack.session_id, "Bearer [REDACTED]");
        assert_eq!(session_id, "Bearer [REDACTED]");
        assert_eq!(agent, "Authorization: Bearer [REDACTED]");
        assert_eq!(source, "source bearer [REDACTED]");
        assert_eq!(kind, "kind bearer [REDACTED]");
        for stored in [session_id, agent, source, kind, job_payload] {
            for secret in [session_secret, agent_secret, source_secret, kind_secret] {
                assert!(!stored.contains(secret), "stored metadata leaked {secret}");
            }
        }

        cleanup_db_files(&path);
    }

    #[test]
    fn recall_events_returns_lexical_matches_without_provider_usage() {
        let path = temp_db_path("recall-events");
        let mut store = Store::open(&path).expect("store opens");

        store
            .capture_event(test_event_with_text(
                "session-1",
                1000,
                "WAL timeout fixed by busy_timeout",
            ))
            .expect("first capture succeeds");
        store
            .capture_event(test_event_with_text(
                "session-1",
                2000,
                "Release checklist updated",
            ))
            .expect("second capture succeeds");

        let result = store.recall_events("wal busy", 5).expect("recall succeeds");

        assert_eq!(result.mode, "lexical");
        assert!(!result.degraded);
        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].raw_event_id, 1);
        assert_eq!(result.hits[0].session_id, "session-1");
        assert_eq!(result.hits[0].content, "WAL timeout fixed by busy_timeout");

        let provider_rows = store
            .conn
            .query_row("SELECT COUNT(*) FROM provider_usage", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("provider usage count");
        assert_eq!(provider_rows, 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn recall_events_rejects_empty_queries() {
        let path = temp_db_path("recall-empty-query");
        let store = Store::open(&path).expect("store opens");

        let err = store.recall_events("?!", 5).expect_err("recall fails");

        assert!(matches!(err, StoreError::InvalidRecallQuery));

        cleanup_db_files(&path);
    }

    #[test]
    fn recall_events_treats_operator_like_terms_as_literals() {
        let path = temp_db_path("recall-operator-like-terms");
        let mut store = Store::open(&path).expect("store opens");
        store
            .capture_event(test_event_with_text("session-1", 1000, "AND OR NOT"))
            .expect("capture succeeds");

        let result = store.recall_events("AND", 5).expect("recall succeeds");

        assert_eq!(result.hits.len(), 1);
        assert_eq!(result.hits[0].content, "AND OR NOT");

        cleanup_db_files(&path);
    }

    fn test_event(session_id: &str, ts_ms: i64) -> NewRawEvent {
        test_event_with_text(session_id, ts_ms, "busy_timeout fixed WAL contention")
    }

    fn test_event_with_text(session_id: &str, ts_ms: i64, text: &str) -> NewRawEvent {
        NewRawEvent {
            session_id: session_id.to_string(),
            agent: "claude".to_string(),
            source: "tool_result".to_string(),
            kind: "observation".to_string(),
            payload: serde_json::json!({"text": text}),
            provenance: serde_json::json!({}),
            ts_ms,
        }
    }

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
}
