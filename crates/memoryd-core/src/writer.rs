//! Single-writer store actor (ARCHITECTURE-PLAN §7.1 / U5).
//!
//! All hot-path writers — HTTP capture, auth audit, and the embed worker —
//! route their writes through one [`Writer`] thread that owns the only
//! mutable [`Store`], serializing SQLite writes behind an mpsc queue. The
//! dream loop intentionally stays a direct low-frequency writer instead:
//! `consolidate_pending` runs inference inside `Store` methods, and parking
//! that work on the writer thread would serialize capture latency behind it.

use std::path::Path;
use std::sync::mpsc;
use std::thread;

use crate::store::{Store, StoreError};

/// A unit of work executed on the writer thread against its owned `Store`.
type Job = Box<dyn FnOnce(&mut Store) + Send>;

/// The single-writer store actor. [`Writer::spawn`] owns the only mutable
/// `Store`; everything else talks to it through cloned [`WriterHandle`]s.
pub struct Writer;

impl Writer {
    /// Open a `Store` at `db_path` and spawn the writer thread that owns it.
    ///
    /// The thread drains jobs in submission order and exits cleanly once
    /// every [`WriterHandle`] (sender) has been dropped.
    pub fn spawn(
        db_path: impl AsRef<Path>,
    ) -> Result<(WriterHandle, thread::JoinHandle<()>), StoreError> {
        let mut store = Store::open(db_path)?;
        let (sender, receiver) = mpsc::channel::<Job>();
        let join = thread::spawn(move || {
            for job in receiver {
                job(&mut store);
            }
        });
        Ok((WriterHandle(sender), join))
    }
}

/// Cloneable handle that submits closures to the writer thread and waits
/// for their result on a per-call reply channel.
#[derive(Clone)]
pub struct WriterHandle(mpsc::Sender<Job>);

impl WriterHandle {
    /// Run `f` on the writer thread's `Store` and return its result.
    ///
    /// Blocks until the writer executes the closure. If the writer thread is
    /// gone (send or reply channel disconnected), returns
    /// [`StoreError::WriterGone`].
    pub fn exec<R, F>(&self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static,
    {
        let (reply_tx, reply_rx) = mpsc::channel();
        self.0
            .send(Box::new(move |store: &mut Store| {
                // A dropped receiver means the caller gave up waiting; the
                // write itself still happened, so ignore the send failure.
                let _ = reply_tx.send(f(store));
            }))
            .map_err(|_| StoreError::WriterGone)?;
        reply_rx.recv().map_err(|_| StoreError::WriterGone)
    }
}

/// Uniform write access: code generic over `StoreAccess` runs identically
/// against a direct `&mut Store` (inline) or a [`WriterHandle`] (actor).
pub trait StoreAccess {
    fn run<R, F>(&mut self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static;
}

impl StoreAccess for Store {
    fn run<R, F>(&mut self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static,
    {
        Ok(f(self))
    }
}

impl StoreAccess for WriterHandle {
    fn run<R, F>(&mut self, f: F) -> Result<R, StoreError>
    where
        F: FnOnce(&mut Store) -> R + Send + 'static,
        R: Send + 'static,
    {
        self.exec(f)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::NewRawEvent;
    use std::path::{Path, PathBuf};
    use std::sync::{Arc, Mutex};
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

    #[test]
    fn writer_executes_jobs_in_submission_order() {
        let path = temp_db_path("writer-order");
        let (handle, join) = Writer::spawn(&path).expect("writer spawns");

        let seen = Arc::new(Mutex::new(Vec::new()));

        let first = Arc::clone(&seen);
        handle
            .exec(move |_store| first.lock().expect("lock").push(1))
            .expect("first job runs");

        let second = Arc::clone(&seen);
        handle
            .exec(move |_store| second.lock().expect("lock").push(2))
            .expect("second job runs");

        assert_eq!(*seen.lock().expect("lock"), vec![1, 2]);

        drop(handle);
        join.join().expect("writer thread exits cleanly");
        cleanup_db_files(&path);
    }

    #[test]
    fn writer_exec_returns_closure_result() {
        let path = temp_db_path("writer-result");
        let (handle, join) = Writer::spawn(&path).expect("writer spawns");

        let ack = handle
            .exec(|store| {
                store.capture_event(NewRawEvent {
                    session_id: "s1".to_string(),
                    agent: "claude".to_string(),
                    source: "tool_result".to_string(),
                    kind: "observation".to_string(),
                    payload: serde_json::json!({ "text": "captured via writer actor" }),
                    provenance: serde_json::json!({}),
                    ts_ms: 1000,
                })
            })
            .expect("exec reaches writer")
            .expect("capture succeeds");

        assert_eq!(ack.raw_event_id, 1);
        assert_eq!(ack.session_id, "s1");
        assert!(ack.enqueued_job_id.is_some());
        assert!(!ack.degraded);

        drop(handle);
        join.join().expect("writer thread exits cleanly");
        cleanup_db_files(&path);
    }

    #[test]
    fn writer_exits_when_handles_drop() {
        let path = temp_db_path("writer-exit");
        let (handle, join) = Writer::spawn(&path).expect("writer spawns");

        drop(handle);
        join.join().expect("writer thread exits cleanly");
        cleanup_db_files(&path);
    }
}
