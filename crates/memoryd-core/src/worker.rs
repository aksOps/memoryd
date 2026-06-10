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
            Ok(vectors) => match vectors.into_iter().next() {
                Some(vector) if !vector.is_empty() => {
                    store.complete_embed_job(
                        job.job_id,
                        job.raw_event_id,
                        crate::store::EmbedProvider {
                            adapter_id: adapter.id(),
                            model_id: adapter.model_id(),
                        },
                        &vector,
                        prompt_token_estimate(&job.content),
                        now_ms,
                    )?;
                    report.completed += 1;
                }
                _ => {
                    // An adapter that returns no usable vector must not silently
                    // persist a zero-dim embedding and mark the job done.
                    store.fail_job(
                        job.job_id,
                        job.attempts,
                        "adapter returned no embedding vector",
                        now_ms,
                        caps.job_max_attempts,
                        caps.job_backoff_base_ms,
                    )?;
                    report.failed += 1;
                }
            },
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

    fn job_state(path: &Path) -> String {
        let conn = rusqlite::Connection::open(path).expect("open db for state");
        conn.query_row("SELECT state FROM jobs LIMIT 1", [], |row| row.get(0))
            .expect("job state")
    }

    struct FailingAdapter;

    impl ProviderAdapter for FailingAdapter {
        fn id(&self) -> &'static str {
            "null"
        }
        fn model_id(&self) -> &str {
            "failing"
        }
        fn reachable(&self) -> bool {
            true
        }
        fn embed(&self, _texts: &[String]) -> Result<Vec<Vec<f32>>, AdapterError> {
            Err(AdapterError::Embed("synthetic adapter failure".to_string()))
        }
    }

    #[test]
    fn tick_defers_job_when_adapter_fails() {
        let path = temp_db_path("worker-fail");
        let mut store = Store::open(&path).expect("store opens");
        capture_text(&mut store, "s1", 1000, "will fail to embed");

        let caps = Caps::small();
        let report =
            tick_embed(&mut store, &FailingAdapter, &caps, 9_000_000_000_000).expect("tick runs");
        assert_eq!(report.leased, 1);
        assert_eq!(report.completed, 0);
        assert_eq!(report.failed, 1);
        assert_eq!(embeddings_count(&path), 0);
        assert_eq!(job_state(&path), "deferred");

        cleanup_db_files(&path);
    }
}
