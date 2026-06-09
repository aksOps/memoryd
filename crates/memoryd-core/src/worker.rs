//! Embed worker: the single active worker in the M3 background plane.
//!
//! One `tick_embed` call is one governor-bounded batch — it leases up to
//! `worker_concurrency` ready embed jobs, embeds each through the provider
//! adapter, and persists the embedding + usage ledger row, completing or
//! deferring/dead-lettering the job per its outcome.

use crate::adapters::{AdapterError, ProviderAdapter, prompt_token_estimate};
use crate::config::Caps;
use crate::store::{Store, StoreError};

/// Per-tick counts for observability.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TickReport {
    pub leased: usize,
    pub completed: usize,
    pub failed: usize,
}

/// Lease and process one governor-bounded batch of embed jobs.
pub fn tick_embed<A: ProviderAdapter>(
    store: &mut Store,
    adapter: &A,
    caps: &Caps,
    now_ms: i64,
) -> Result<TickReport, StoreError> {
    let limit = caps.worker_concurrency.max(1);
    let visibility_ms =
        i64::try_from(caps.lease_visibility_secs.saturating_mul(1000)).unwrap_or(i64::MAX);
    let leased = store.lease_embed_jobs(limit, now_ms, visibility_ms)?;

    let mut report = TickReport {
        leased: leased.len(),
        ..Default::default()
    };

    for job in leased {
        match adapter.embed(std::slice::from_ref(&job.content)) {
            Ok(vectors) => {
                let vector = vectors.first().map(Vec::as_slice).unwrap_or_default();
                store.complete_embed_job(
                    job.job_id,
                    job.raw_event_id,
                    adapter.model_id(),
                    vector,
                    prompt_token_estimate(&job.content),
                    now_ms,
                )?;
                report.completed += 1;
            }
            Err(AdapterError::Embed(message)) => {
                store.fail_job(
                    job.job_id,
                    job.attempts,
                    &message,
                    now_ms,
                    caps.job_max_attempts,
                    caps.job_backoff_base_ms,
                )?;
                report.failed += 1;
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::NullAdapter;
    use crate::store::NewRawEvent;
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
            let _ = std::fs::remove_file(file);
        }
    }

    fn capture_text(store: &mut Store, session: &str, ts: i64, text: &str) {
        store
            .capture_event(NewRawEvent {
                session_id: session.to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({ "text": text }),
                provenance: serde_json::json!({}),
                ts_ms: ts,
            })
            .expect("capture succeeds");
    }

    fn embeddings_count(path: &Path) -> i64 {
        let conn = rusqlite::Connection::open(path).expect("open db for count");
        conn.query_row("SELECT COUNT(*) FROM embeddings", [], |row| row.get(0))
            .expect("embeddings count")
    }

    #[test]
    fn tick_processes_pending_jobs_up_to_concurrency() {
        let path = temp_db_path("worker-tick");
        let mut store = Store::open(&path).expect("store opens");
        capture_text(&mut store, "s1", 1000, "alpha embed candidate");
        capture_text(&mut store, "s1", 1001, "beta embed candidate");
        capture_text(&mut store, "s1", 1002, "gamma embed candidate");

        let adapter = NullAdapter::new();
        let mut caps = Caps::small();
        caps.worker_concurrency = 2;

        let first = tick_embed(&mut store, &adapter, &caps, 9_000_000_000_000).expect("tick runs");
        assert_eq!(first.leased, 2);
        assert_eq!(first.completed, 2);
        assert_eq!(first.failed, 0);
        assert_eq!(embeddings_count(&path), 2);

        let second = tick_embed(&mut store, &adapter, &caps, 9_000_000_000_001).expect("tick runs");
        assert_eq!(second.leased, 1);
        assert_eq!(second.completed, 1);
        assert_eq!(embeddings_count(&path), 3);

        cleanup_db_files(&path);
    }

    #[test]
    fn tick_with_no_jobs_is_noop() {
        let path = temp_db_path("worker-empty");
        let mut store = Store::open(&path).expect("store opens");
        let adapter = NullAdapter::new();
        let caps = Caps::small();

        let report = tick_embed(&mut store, &adapter, &caps, 9_000_000_000_000).expect("tick runs");
        assert_eq!(report, TickReport::default());

        cleanup_db_files(&path);
    }
}
