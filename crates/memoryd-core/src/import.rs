//! Historic import: parse foreign export formats into stage-able units.
//!
//! M5 scope is the **generic JSONL** format only (source-specific importers are
//! deferred until the JSONL path is stable, per the milestone plan). Parsing here
//! is pure and network-free; staging into `raw_events` lives in [`crate::store`].

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
        }
    }
}

impl std::error::Error for ImportError {}

const DEFAULT_FIELD: &str = "import";

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
