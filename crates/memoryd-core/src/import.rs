//! Historic import: parse foreign export formats into stage-able units.
//!
//! Generic JSONL parsing plus the shared primitives (truncation, timestamps,
//! content hashing) used by the source-specific parsers in [`crate::importers`].
//! Parsing here is pure and network-free; staging into `raw_events` lives in
//! [`crate::store`].

use std::fmt;

/// One parsed import record, before staging. Mirrors the capture inputs that
/// [`crate::store::Store::import_jsonl`] feeds into the normal raw-event path.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportUnit {
    pub text: String,
    pub session_id: String,
    pub agent: String,
    pub source: String,
    /// Event timestamp in unix ms; `None` defers to a per-batch default.
    pub ts_ms: Option<i64>,
}

/// Result of an `import` run: cumulative batch counters plus the terminal state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ImportSummary {
    pub batch_id: String,
    pub source: String,
    pub path: String,
    pub total: usize,
    pub processed: usize,
    pub skipped: usize,
    pub state: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImportError {
    /// A non-empty line was not valid JSON or not a JSON object.
    Malformed { line: usize, msg: String },
    /// A record was missing the required non-empty `text` string.
    MissingText { line: usize },
    /// A source database could not be opened or read.
    Db(String),
    /// A source database exists but does not match the layout this importer knows.
    UnsupportedSchema { agent: &'static str, detail: String },
}

impl fmt::Display for ImportError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Malformed { line, msg } => write!(f, "line {line}: {msg}"),
            Self::MissingText { line } => {
                write!(
                    f,
                    "line {line}: missing required non-empty string field \"text\""
                )
            }
            Self::Db(msg) => write!(f, "source database error: {msg}"),
            Self::UnsupportedSchema { agent, detail } => {
                write!(f, "unsupported {agent} database schema: {detail}")
            }
        }
    }
}

impl std::error::Error for ImportError {}

const DEFAULT_FIELD: &str = "import";

/// Per-unit character cap applied by the source-specific importers
/// ([`crate::importers`]): long tool dumps truncate rather than bloating the store.
pub const IMPORT_TEXT_CAP: usize = 4_000;

/// Char-boundary-safe truncation with ellipsis (same semantics as the capture
/// hook's truncation): at most `max_chars` characters are kept, and a `…` is
/// appended only when something was cut.
pub fn truncate_chars(text: &str, max_chars: usize) -> String {
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

/// Lenient std-only ISO-8601 parser returning unix epoch milliseconds.
///
/// Accepts `YYYY-MM-DDTHH:MM:SS` (a space instead of `T` also works — some
/// tools emit that), an optional fractional-seconds part of 1-9 digits (kept at
/// millisecond precision), and an optional `Z` / `±HH:MM` offset (applied; no
/// suffix means UTC). Anything else returns `None` — import timestamps are
/// best-effort, so callers fall back to a batch default rather than failing.
pub fn parse_iso8601_ms(s: &str) -> Option<i64> {
    let bytes = s.trim().as_bytes();
    if bytes.len() < 19 {
        return None;
    }
    let digits = |range: std::ops::Range<usize>| -> Option<i64> {
        let mut value = 0i64;
        for &byte in bytes.get(range)? {
            if !byte.is_ascii_digit() {
                return None;
            }
            value = value * 10 + i64::from(byte - b'0');
        }
        Some(value)
    };

    if bytes[4] != b'-'
        || bytes[7] != b'-'
        || (bytes[10] != b'T' && bytes[10] != b' ')
        || bytes[13] != b':'
        || bytes[16] != b':'
    {
        return None;
    }
    let year = digits(0..4)?;
    let month = digits(5..7)?;
    let day = digits(8..10)?;
    let hour = digits(11..13)?;
    let minute = digits(14..16)?;
    let second = digits(17..19)?;
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) {
        return None;
    }
    if hour > 23 || minute > 59 || second > 59 {
        return None;
    }

    // Optional fraction: 1-9 digits; only millisecond precision is kept.
    let mut idx = 19;
    let mut millis = 0i64;
    if bytes.get(idx) == Some(&b'.') {
        idx += 1;
        let start = idx;
        while idx < bytes.len() && bytes[idx].is_ascii_digit() {
            idx += 1;
        }
        let count = idx - start;
        if count == 0 || count > 9 {
            return None;
        }
        for position in 0..3 {
            let digit = if start + position < idx {
                i64::from(bytes[start + position] - b'0')
            } else {
                0
            };
            millis = millis * 10 + digit;
        }
    }

    // Optional offset suffix; the remainder must be consumed exactly.
    let rest = &bytes[idx..];
    let offset_minutes = if rest.is_empty() || rest == b"Z" || rest == b"z" {
        0
    } else if rest.len() == 6 && (rest[0] == b'+' || rest[0] == b'-') && rest[3] == b':' {
        let offset_hours = digits(idx + 1..idx + 3)?;
        let offset_mins = digits(idx + 4..idx + 6)?;
        if offset_hours > 23 || offset_mins > 59 {
            return None;
        }
        let total = offset_hours * 60 + offset_mins;
        if rest[0] == b'+' { total } else { -total }
    } else {
        return None;
    };

    let days = days_from_civil(year, month, day);
    let seconds = days * 86_400 + hour * 3_600 + minute * 60 + second - offset_minutes * 60;
    Some(seconds * 1_000 + millis)
}

/// Days since 1970-01-01 for a proleptic-Gregorian civil date
/// (Howard Hinnant's `days_from_civil` algorithm).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let shifted_year = if month <= 2 { year - 1 } else { year };
    let era = shifted_year.div_euclid(400);
    let year_of_era = shifted_year.rem_euclid(400);
    let day_of_year = (153 * ((month + 9) % 12) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Parse a generic JSONL document: one JSON object per non-blank line.
///
/// Required: `text` (non-empty string). Optional: `session_id`, `agent`, `source`
/// (default `"import"`), and `ts_ms` (or `ts`) as an integer. Blank/whitespace-only
/// lines are skipped. Order is preserved (file order = stage order).
pub fn parse_jsonl(contents: &str) -> Result<Vec<ImportUnit>, ImportError> {
    let mut units = Vec::new();
    for (idx, raw_line) in contents.lines().enumerate() {
        let line_no = idx + 1;
        let trimmed = raw_line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let value: serde_json::Value =
            serde_json::from_str(trimmed).map_err(|err| ImportError::Malformed {
                line: line_no,
                msg: err.to_string(),
            })?;
        let serde_json::Value::Object(obj) = value else {
            return Err(ImportError::Malformed {
                line: line_no,
                msg: "expected a JSON object".to_string(),
            });
        };

        let text = match obj.get("text").and_then(serde_json::Value::as_str) {
            Some(text) if !text.trim().is_empty() => text.to_string(),
            _ => return Err(ImportError::MissingText { line: line_no }),
        };
        let string_field = |key: &str| {
            obj.get(key)
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.trim().is_empty())
                .map_or_else(|| DEFAULT_FIELD.to_string(), str::to_string)
        };
        let ts_ms = obj
            .get("ts_ms")
            .or_else(|| obj.get("ts"))
            .and_then(serde_json::Value::as_i64);

        units.push(ImportUnit {
            text,
            session_id: string_field("session_id"),
            agent: string_field("agent"),
            source: string_field("source"),
            ts_ms,
        });
    }
    Ok(units)
}

/// Normalize text for content hashing: trim ends and collapse internal whitespace
/// runs to a single space. ASCII-level only — NFC Unicode normalization would pull
/// in a dependency the project does not allow.
fn normalize(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// Stable dedup key for an imported unit: FNV-1a 64-bit over
/// `source_tag \0 normalized_text`, returned as 8 little-endian bytes.
///
/// FNV-1a (not BLAKE3 as the design sketch suggested) keeps the dependency set at
/// `rusqlite` + `serde_json`; it matches the inline hash already used for query-cache
/// keys. Collision risk is negligible at personal-archive scale. The caller passes the
/// post-redaction text, so the key matches stored content and never derives from a secret.
pub fn content_hash(source_tag: &str, text: &str) -> Vec<u8> {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    let mut mix = |bytes: &[u8]| {
        for byte in bytes {
            hash ^= u64::from(*byte);
            hash = hash.wrapping_mul(0x100_0000_01b3);
        }
    };
    mix(source_tag.as_bytes());
    mix(&[0]);
    mix(normalize(text).as_bytes());
    hash.to_le_bytes().to_vec()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_jsonl_reads_objects_and_skips_blank_lines() {
        let doc = "\n{\"text\":\"first\"}\n  \n{\"text\":\"second\",\"session_id\":\"s1\",\"ts_ms\":42}\n";
        let units = parse_jsonl(doc).expect("parses");
        assert_eq!(units.len(), 2);
        assert_eq!(units[0].text, "first");
        assert_eq!(units[0].session_id, "import");
        assert_eq!(units[0].agent, "import");
        assert_eq!(units[0].ts_ms, None);
        assert_eq!(units[1].text, "second");
        assert_eq!(units[1].session_id, "s1");
        assert_eq!(units[1].ts_ms, Some(42));
    }

    #[test]
    fn parse_jsonl_accepts_ts_alias() {
        let units = parse_jsonl("{\"text\":\"x\",\"ts\":99}").expect("parses");
        assert_eq!(units[0].ts_ms, Some(99));
    }

    #[test]
    fn parse_jsonl_rejects_missing_text_with_line_number() {
        let err = parse_jsonl("{\"text\":\"ok\"}\n{\"session_id\":\"s\"}").unwrap_err();
        assert_eq!(err, ImportError::MissingText { line: 2 });
    }

    #[test]
    fn parse_jsonl_rejects_blank_text() {
        let err = parse_jsonl("{\"text\":\"   \"}").unwrap_err();
        assert_eq!(err, ImportError::MissingText { line: 1 });
    }

    #[test]
    fn parse_jsonl_reports_bad_json_line() {
        let err = parse_jsonl("{\"text\":\"ok\"}\nnot json").unwrap_err();
        assert!(matches!(err, ImportError::Malformed { line: 2, .. }));
    }

    #[test]
    fn parse_jsonl_rejects_non_object() {
        let err = parse_jsonl("[1,2,3]").unwrap_err();
        assert!(matches!(err, ImportError::Malformed { line: 1, .. }));
    }

    #[test]
    fn parse_iso8601_ms_handles_utc_offsets_and_millis() {
        // 2024-01-15T12:30:45Z is unix second 1_705_321_845.
        assert_eq!(parse_iso8601_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(
            parse_iso8601_ms("2024-01-15T12:30:45Z"),
            Some(1_705_321_845_000)
        );
        assert_eq!(
            parse_iso8601_ms("2024-01-15T12:30:45.123Z"),
            Some(1_705_321_845_123)
        );
        // Sub-millisecond precision is truncated to milliseconds.
        assert_eq!(
            parse_iso8601_ms("2024-01-15T12:30:45.123456789Z"),
            Some(1_705_321_845_123)
        );
        // Short fractions are right-padded: .5 == 500 ms.
        assert_eq!(
            parse_iso8601_ms("2024-01-15T12:30:45.5Z"),
            Some(1_705_321_845_500)
        );
        // Positive and negative offsets fold back to the same UTC instant.
        assert_eq!(
            parse_iso8601_ms("2024-01-15T13:30:45+01:00"),
            Some(1_705_321_845_000)
        );
        assert_eq!(
            parse_iso8601_ms("2024-01-15T07:00:45-05:30"),
            Some(1_705_321_845_000)
        );
        // No suffix means UTC; a space separator is tolerated.
        assert_eq!(
            parse_iso8601_ms("2024-01-15 12:30:45"),
            Some(1_705_321_845_000)
        );
        // Pre-epoch dates are negative milliseconds.
        assert_eq!(parse_iso8601_ms("1969-12-31T23:59:59Z"), Some(-1_000));
    }

    #[test]
    fn parse_iso8601_ms_returns_none_on_garbage() {
        for input in [
            "",
            "not a date",
            "2024-01-15",                      // date only
            "2024-13-01T00:00:00Z",            // month out of range
            "2024-01-32T00:00:00Z",            // day out of range
            "2024-01-15T25:00:00Z",            // hour out of range
            "2024-01-15T12:61:00Z",            // minute out of range
            "2024-01-15T12:30:45.Z",           // empty fraction
            "2024-01-15T12:30:45.1234567890Z", // >9 fraction digits
            "2024-01-15T12:30:45+1:00",        // malformed offset
            "2024-01-15T12:30:45+25:00",       // offset hour out of range
            "2024-01-15T12:30:45Zjunk",        // trailing garbage
            "2024-01-15X12:30:45",             // bad separator
            "２０２４-01-15T12:30:45Z",        // non-ASCII digits
        ] {
            assert_eq!(parse_iso8601_ms(input), None, "input: {input:?}");
        }
    }

    #[test]
    fn truncate_chars_is_char_boundary_safe() {
        assert_eq!(truncate_chars("hello", 10), "hello");
        assert_eq!(truncate_chars("hello", 5), "hello");
        assert_eq!(truncate_chars("hello", 4), "hell…");
        // Multi-byte characters truncate on char boundaries, never mid-codepoint.
        assert_eq!(truncate_chars("héllo wörld", 2), "hé…");
        assert_eq!(truncate_chars("日本語テキスト", 3), "日本語…");
        assert_eq!(truncate_chars("", 0), "");
        assert_eq!(truncate_chars("ab", 0), "…");
    }

    #[test]
    fn content_hash_is_stable_and_whitespace_insensitive() {
        assert_eq!(
            content_hash("jsonl", "lock  mutex\tcontention"),
            content_hash("jsonl", " lock mutex contention "),
        );
    }

    #[test]
    fn content_hash_differs_on_text_and_source_tag() {
        assert_ne!(content_hash("jsonl", "a"), content_hash("jsonl", "b"));
        assert_ne!(content_hash("jsonl", "a"), content_hash("chatlog", "a"));
        assert_eq!(content_hash("jsonl", "a").len(), 8);
    }
}
