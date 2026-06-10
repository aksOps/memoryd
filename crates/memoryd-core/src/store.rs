use crate::adapters::{AdapterError, ProviderAdapter, prompt_token_estimate};
use crate::dream::{
    ARCHIVE_GRACE_MS, ASSOCIATE_FANOUT_CAP, CO_OCCUR_BASE, CO_OCCUR_GROUP_CAP, CO_OCCUR_REINFORCE,
    SEM_LINK_THRESHOLD, WEAK_LINK_FLOOR, centrality_for, decay_score, half_life_ms, lifecycle_for,
    memory_kind_for, next_decay_at, normalize, score_base, trust_for_source,
};
use crate::import::{ImportError, ImportSummary, ImportUnit, content_hash, parse_jsonl};
use crate::vectorindex::{Candidate, VectorIndex};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 7;

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
/// All-digit runs at least this long are treated as secrets (PANs, numeric
/// tokens). 16 catches full card numbers while leaving unix-millisecond
/// timestamps (13 digits) alone.
const DIGIT_SECRET_MIN_LEN: usize = 16;

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

    /// Import a generic JSONL file into `raw_events`, idempotently and resumably.
    ///
    /// Each non-blank line becomes one `kind='import'` raw event routed through the
    /// same session / FTS / embed-queue path as native capture (no privileged path,
    /// H7). Re-running is safe: already-seen content is skipped by `content_hash`, so
    /// the same row is never written twice. `processed`/`skipped` are reset per run, so
    /// at completion `processed + skipped == total`. When the embed queue is full the
    /// run *pauses* (batch left resumable) rather than dropping work. Only
    /// `source = "jsonl"` is supported in this slice.
    pub fn import_jsonl(
        &mut self,
        source: &str,
        path: &Path,
        max_active_jobs: usize,
    ) -> Result<ImportSummary, StoreError> {
        // Bound memory on a constrained host: the file is read whole before parsing,
        // so reject oversized inputs with a clear message rather than risking an OOM.
        let file_bytes = fs::metadata(path)?.len();
        if file_bytes > MAX_IMPORT_FILE_BYTES {
            return Err(StoreError::Import(format!(
                "import file is {file_bytes} bytes; the limit is {MAX_IMPORT_FILE_BYTES} \
                 bytes - split the source or import it in chunks"
            )));
        }
        let contents = fs::read_to_string(path)?;
        let units = parse_jsonl(&contents)?;
        let path_or_uri = path.display().to_string();
        let now = unix_ms_now();

        let batch_id = self.begin_import_batch(source, &path_or_uri, units.len(), now)?;

        let mut paused = false;
        for unit in &units {
            let outcome = self.stage_import_unit(
                &batch_id,
                source,
                &path_or_uri,
                unit,
                now,
                max_active_jobs,
            )?;
            if matches!(outcome, StageOutcome::Paused) {
                paused = true;
                break;
            }
        }

        let state = if paused { "paused" } else { "completed" };
        let finished_at = if paused { None } else { Some(now) };
        self.conn.execute(
            "UPDATE import_batches SET state = ?1, finished_at = ?2 WHERE id = ?3",
            params![state, finished_at, batch_id],
        )?;

        self.import_summary(&batch_id, source, &path_or_uri)
    }

    /// Find-or-create the `import_batches` row for `(source, path_or_uri)` and reset it
    /// to `staging` with the current unit `total`. The `UNIQUE(source, path_or_uri)`
    /// constraint makes a re-import reuse the same batch row (idempotency / resume).
    fn begin_import_batch(
        &self,
        source: &str,
        path_or_uri: &str,
        total: usize,
        now: i64,
    ) -> Result<String, StoreError> {
        let total = i64::try_from(total).unwrap_or(i64::MAX);
        // Atomic find-or-create: one statement, no SELECT-then-write race. On resume the
        // counters reset so each run reports its own progress (processed + skipped == total
        // at completion); dedup over content_hash - not the counters - prevents double-staging.
        let id: String = self.conn.query_row(
            "INSERT INTO import_batches (id, source, path_or_uri, total, state, started_at)
             VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, 'staging', ?4)
             ON CONFLICT(source, path_or_uri) DO UPDATE SET
                total = excluded.total,
                state = 'staging',
                processed = 0,
                skipped = 0,
                finished_at = NULL
             RETURNING id",
            params![source, path_or_uri, total, now],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Stage one import unit in a single transaction so progress is crash-safe.
    ///
    /// Dedup, the staging insert, and the `import_batches` counter bump all commit
    /// together: an interrupted run never double-stages and never loses count.
    fn stage_import_unit(
        &mut self,
        batch_id: &str,
        source: &str,
        path_or_uri: &str,
        unit: &ImportUnit,
        batch_ts: i64,
        max_active_jobs: usize,
    ) -> Result<StageOutcome, StoreError> {
        // Redact at the boundary (defense-in-depth, s11.7) reusing the capture path's
        // helpers, and key dedup off the *redacted* text so the content_hash matches what
        // is stored and never derives from an unredacted secret.
        let RedactedString {
            value: text,
            redactions: text_redactions,
        } = redact_inline_string_with_count(&unit.text);
        let hash = content_hash(source, &text);

        // IMMEDIATE so the dedup check + staging insert take the write lock up front;
        // concurrent importers then serialize and dedup gracefully instead of racing.
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        let already = tx
            .query_row(
                "SELECT 1 FROM raw_events WHERE content_hash = ?1 AND kind = 'import' LIMIT 1",
                params![hash],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .is_some();
        if already {
            tx.execute(
                "UPDATE import_batches SET skipped = skipped + 1 WHERE id = ?1",
                params![batch_id],
            )?;
            tx.commit()?;
            return Ok(StageOutcome::Skipped);
        }

        let job_limit = i64::try_from(max_active_jobs).unwrap_or(i64::MAX);
        if active_job_count(&tx)? >= job_limit {
            // Queue full: pause without staging so a later run resumes this unit.
            tx.commit()?;
            return Ok(StageOutcome::Paused);
        }

        let RedactedString {
            value: session_id,
            redactions: session_redactions,
        } = redact_inline_string_with_count(&unit.session_id);
        let RedactedString {
            value: agent,
            redactions: agent_redactions,
        } = redact_inline_string_with_count(&unit.agent);
        let RedactedString {
            value: event_source,
            redactions: source_redactions,
        } = redact_inline_string_with_count(&unit.source);
        validate_capture_field("session_id", &session_id)?;
        validate_capture_field("agent", &agent)?;
        validate_capture_field("source", &event_source)?;

        let ts_ms = unit.ts_ms.unwrap_or(batch_ts);
        let payload_value = serde_json::json!({ "text": text });
        let provenance_value = serde_json::json!({
            "import_batch": batch_id,
            "import_source": source,
            "path": path_or_uri,
        });
        let fts_content = capture_fts_content(&payload_value);
        let payload = serde_json::to_string(&payload_value)?;
        let provenance = serde_json::to_string(&provenance_value)?;

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
            "INSERT INTO raw_events
                (session_id, ts, source, kind, payload, provenance, content_hash)
             VALUES (?1, ?2, ?3, 'import', ?4, ?5, ?6)",
            params![
                session_id.as_str(),
                ts_ms,
                event_source.as_str(),
                payload.as_str(),
                provenance.as_str(),
                hash
            ],
        )?;
        let raw_event_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
            params![raw_event_id, fts_content.as_str()],
        )?;
        let job_payload = serde_json::to_string(&serde_json::json!({
            "raw_event_id": raw_event_id,
            "session_id": session_id,
            "source": event_source,
            "kind": "import",
        }))?;
        tx.execute(
            "INSERT INTO jobs (kind, priority, state, payload, scheduled_at)
             VALUES ('embed', 100, 'pending', ?1, ?2)",
            params![job_payload.as_str(), batch_ts],
        )?;
        let raw_event_ref = raw_event_id.to_string();
        let stage_detail = serde_json::to_string(&serde_json::json!({
            "import_batch": batch_id,
            "session_id": session_id,
            "source": event_source,
        }))?;
        insert_audit_log(
            &tx,
            AUDIT_ACTOR,
            "import.stage",
            "raw_event",
            Some(raw_event_ref.as_str()),
            Some(stage_detail.as_str()),
            batch_ts,
        )?;
        let redactions =
            text_redactions + session_redactions + agent_redactions + source_redactions;
        if redactions > 0 {
            let redaction_detail = serde_json::to_string(&serde_json::json!({
                "redactions": redactions,
                "replacement": REDACTED,
            }))?;
            insert_audit_log(
                &tx,
                AUDIT_ACTOR,
                "redaction.apply",
                "raw_event",
                Some(raw_event_ref.as_str()),
                Some(redaction_detail.as_str()),
                batch_ts,
            )?;
        }
        tx.execute(
            "UPDATE import_batches SET processed = processed + 1 WHERE id = ?1",
            params![batch_id],
        )?;
        tx.commit()?;
        Ok(StageOutcome::Staged)
    }

    fn import_summary(
        &self,
        batch_id: &str,
        source: &str,
        path_or_uri: &str,
    ) -> Result<ImportSummary, StoreError> {
        let (total, processed, skipped, state): (i64, i64, i64, String) = self.conn.query_row(
            "SELECT total, processed, skipped, state FROM import_batches WHERE id = ?1",
            params![batch_id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
        )?;
        Ok(ImportSummary {
            batch_id: batch_id.to_string(),
            source: source.to_string(),
            path: path_or_uri.to_string(),
            total: usize::try_from(total).unwrap_or(0),
            processed: usize::try_from(processed).unwrap_or(0),
            skipped: usize::try_from(skipped).unwrap_or(0),
            state,
        })
    }

    /// Open a new `dream_runs` row in the `running` state; returns its id.
    pub fn create_dream_run(&self, trigger: &str, now_ms: i64) -> Result<String, StoreError> {
        let id: String = self.conn.query_row(
            "INSERT INTO dream_runs (id, trigger, started_at, status)
             VALUES (lower(hex(randomblob(16))), ?1, ?2, 'running')
             RETURNING id",
            params![trigger, now_ms],
            |row| row.get(0),
        )?;
        Ok(id)
    }

    /// Finalize a `dream_runs` row with its accounting and terminal status.
    pub fn finish_dream_run(
        &self,
        run_id: &str,
        finished_at: i64,
        jobs_run: i64,
        memories_touched: i64,
        tokens_used: i64,
        status: &str,
    ) -> Result<(), StoreError> {
        self.conn.execute(
            "UPDATE dream_runs SET finished_at = ?1, jobs_run = ?2, memories_touched = ?3,
                tokens_used = ?4, status = ?5 WHERE id = ?6",
            params![
                finished_at,
                jobs_run,
                memories_touched,
                tokens_used,
                status,
                run_id
            ],
        )?;
        Ok(())
    }

    /// Consolidate up to `limit` pending `raw_events` into durable `memories`.
    ///
    /// Exact-normalized-text duplicates within the batch collapse into one memory
    /// (lexical dedup-cluster); each new memory gets an immutable `memory_versions` v1
    /// citing its source raw_event ids. The LLM summary is used only when a metered
    /// adapter is configured *and* the per-run spend stays within `budget_usd`;
    /// otherwise a deterministic lexical representative is used (the shipped `null`
    /// adapter always takes that path). The whole batch commits in one transaction, so
    /// a wall-clock cut between batches never leaves a half-consolidated cluster.
    pub(crate) fn consolidate_pending<A: ProviderAdapter>(
        &mut self,
        adapter: &A,
        budget_usd: f64,
        window_spend: &mut f64,
        run_id: &str,
        limit: usize,
        now_ms: i64,
    ) -> Result<ConsolidateBatch, StoreError> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(i64, String, String, String, String)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, payload, source, kind, session_id FROM raw_events
                 WHERE consolidated_at IS NULL ORDER BY id LIMIT ?1",
            )?;
            let mapped = stmt.query_map(params![limit_i], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            return Ok(ConsolidateBatch {
                memories_created: 0,
                raw_consumed: 0,
                tokens: 0,
                budget_hit: false,
            });
        }
        let raw_consumed = rows.len();

        // Cluster by normalized text, preserving first-seen order for determinism.
        let mut order: Vec<String> = Vec::new();
        let mut clusters: HashMap<String, Cluster> = HashMap::new();
        for (id, payload, source, kind, session) in rows {
            let text = raw_event_text(&payload);
            let key = normalize(&text);
            let entry = clusters.entry(key.clone()).or_insert_with(|| {
                order.push(key.clone());
                Cluster {
                    text: text.clone(),
                    source: source.clone(),
                    kind: kind.clone(),
                    session: session.clone(),
                    ids: Vec::new(),
                }
            });
            entry.ids.push(id);
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut memories_created = 0usize;
        let mut tokens = 0i64;
        let mut budget_hit = false;
        for key in &order {
            let cluster = &clusters[key];
            let price = adapter.usd_per_1k_prompt_tokens();
            let tokens_est = prompt_token_estimate(&cluster.text);
            let content = if price > 0.0 {
                let est_cost = tokens_est as f64 / 1000.0 * price;
                if *window_spend + est_cost <= budget_usd {
                    match adapter.summarize(std::slice::from_ref(&cluster.text))? {
                        Some(summary) => {
                            tx.execute(
                                "INSERT INTO provider_usage
                                    (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
                                 VALUES (?1, ?2, ?3, 'complete', ?4, 0, ?5, NULL)",
                                params![now_ms, adapter.id(), adapter.model_id(), tokens_est, est_cost],
                            )?;
                            *window_spend += est_cost;
                            tokens += tokens_est;
                            summary
                        }
                        None => cluster.text.clone(),
                    }
                } else {
                    // Spend cap would be exceeded: degrade this and subsequent clusters.
                    budget_hit = true;
                    cluster.text.clone()
                }
            } else {
                // Free / null adapter: try summarize (None for null), no spend impact.
                adapter
                    .summarize(std::slice::from_ref(&cluster.text))?
                    .unwrap_or_else(|| cluster.text.clone())
            };

            let mem_kind = memory_kind_for(&cluster.kind);
            let trust = trust_for_source(&cluster.source, &cluster.kind);
            let decay_at = half_life_ms(mem_kind).map(|hl| now_ms + hl);
            let base = score_base(1.0, 0, trust, "active", 0.0);
            let ids_csv = cluster
                .ids
                .iter()
                .map(i64::to_string)
                .collect::<Vec<_>>()
                .join(",");
            let reason = format!("consolidate:dedup-cluster raw_events=[{ids_csv}]");

            let mem_id: String =
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            let ver_id: String =
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            tx.execute(
                "INSERT INTO memories
                    (id, current_version_id, kind, content, lifecycle_state, relevance_score,
                     last_accessed_at, access_count, decay_at, created_at,
                     source_trust, decay_score, decay_recomputed_at, source_session)
                 VALUES (?1, ?2, ?3, ?4, 'active', ?5, ?6, 0, ?7, ?6, ?8, 1.0, ?6, ?9)",
                params![
                    mem_id,
                    ver_id,
                    mem_kind,
                    content,
                    base,
                    now_ms,
                    decay_at,
                    trust,
                    cluster.session
                ],
            )?;
            tx.execute(
                "INSERT INTO memories_fts (memory_id, content) VALUES (?1, ?2)",
                params![mem_id, content],
            )?;
            tx.execute(
                "INSERT INTO memory_versions
                    (id, memory_id, version_no, content, reason, created_by_job, created_at)
                 VALUES (?1, ?2, 1, ?3, ?4, NULL, ?5)",
                params![ver_id, mem_id, content, reason, now_ms],
            )?;
            let detail = serde_json::to_string(&serde_json::json!({
                "dream_run": run_id,
                "kind": mem_kind,
                "raw_events": cluster.ids.len(),
            }))?;
            insert_audit_log(
                &tx,
                AUDIT_ACTOR,
                "consolidate",
                "memory",
                Some(mem_id.as_str()),
                Some(detail.as_str()),
                now_ms,
            )?;
            for id in &cluster.ids {
                tx.execute(
                    "UPDATE raw_events SET consolidated_at = ?1 WHERE id = ?2",
                    params![now_ms, id],
                )?;
            }
            memories_created += 1;
        }
        tx.commit()?;
        Ok(ConsolidateBatch {
            memories_created,
            raw_consumed,
            tokens,
            budget_hit,
        })
    }

    /// Recompute decay + lifecycle for up to `limit` *due* memories (those whose
    /// `decay_at` checkpoint has passed), scan-free via `memories_decay_due`. State is
    /// a pure function of age, so access (which resets `last_accessed_at`) revives a
    /// memory on the next pass; `decay_at` is advanced to the next transition so a row
    /// is never re-selected within a run. One transaction per batch.
    pub(crate) fn decay_due(
        &mut self,
        limit: usize,
        now_ms: i64,
    ) -> Result<DecayBatch, StoreError> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        let rows: Vec<(String, String, i64, i64, f64, f64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, COALESCE(last_accessed_at, created_at), access_count,
                        source_trust, centrality
                 FROM memories
                 WHERE lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')
                   AND decay_at IS NOT NULL AND decay_at <= ?1
                 ORDER BY decay_at ASC LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![now_ms, limit_i], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                    row.get::<_, i64>(3)?,
                    row.get::<_, f64>(4)?,
                    row.get::<_, f64>(5)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if rows.is_empty() {
            return Ok(DecayBatch { touched: 0 });
        }

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut touched = 0usize;
        for (id, kind, last_accessed, access_count, trust, centrality) in &rows {
            let Some(hl) = half_life_ms(kind) else {
                continue;
            };
            let dt = now_ms - *last_accessed;
            let score = decay_score(dt, hl);
            let state = lifecycle_for(score, dt, hl, ARCHIVE_GRACE_MS);
            let new_decay_at = next_decay_at(state, *last_accessed, hl, ARCHIVE_GRACE_MS);
            let base = score_base(score, *access_count, *trust, state, *centrality);
            tx.execute(
                "UPDATE memories SET decay_score = ?1, relevance_score = ?2, lifecycle_state = ?3,
                    decay_at = ?4, decay_recomputed_at = ?5 WHERE id = ?6",
                params![score, base, state, new_decay_at, now_ms, id],
            )?;
            touched += 1;
        }
        let detail = serde_json::to_string(&serde_json::json!({ "memories_touched": touched }))?;
        insert_audit_log(
            &tx,
            AUDIT_ACTOR,
            "decay",
            "memory",
            None,
            Some(detail.as_str()),
            now_ms,
        )?;
        tx.commit()?;
        Ok(DecayBatch { touched })
    }

    /// Build/reinforce/prune the weighted association graph over recall-eligible
    /// memories (ARCHITECTURE-PLAN §9.4, §21.10). Two link signals:
    ///
    /// * **co-occurrence** (deterministic, works for the no-spend `null` adapter):
    ///   memories sharing a `source_session` are linked; re-observation reinforces
    ///   `weight` toward 1.0.
    /// * **embedding similarity** (`semantic`): only when the adapter carries a usable
    ///   semantic signal (`embeds_semantically`); pairwise cosine within a bounded
    ///   window. The `null` adapter skips this path entirely.
    ///
    /// Links are stored **symmetrically** (both directions) so the recall hop, the
    /// per-node fan-out cap, and centrality are all clean `src_memory_id` operations.
    /// Growth is bounded by the fan-out cap (≤ `ASSOCIATE_FANOUT_CAP` per node); weak
    /// links (`weight < WEAK_LINK_FLOOR`) are pruned. Centrality is recomputed for the
    /// batch's candidates and folded into `relevance_score`; a memory that gains a link
    /// transitions `active → associated`. One IMMEDIATE transaction.
    pub(crate) fn associate_pending<A: ProviderAdapter>(
        &mut self,
        adapter: &A,
        _run_started_ms: i64,
        now_ms: i64,
    ) -> Result<AssociateBatch, StoreError> {
        struct Cand {
            id: String,
            content: String,
            session: Option<String>,
            decay_score: f64,
            access_count: i64,
            trust: f64,
            state: String,
        }
        // Phase A (no transaction): load candidates, then compute memory embeddings via
        // the adapter (a non-DB call) so the write transaction below holds no provider latency.
        let cands: Vec<Cand> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, content, source_session, decay_score, access_count, source_trust,
                        lifecycle_state
                 FROM memories
                 WHERE lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')
                 ORDER BY created_at ASC, id ASC LIMIT ?1",
            )?;
            let mapped = stmt.query_map(params![crate::dream::ASSOCIATE_BATCH as i64], |row| {
                Ok(Cand {
                    id: row.get(0)?,
                    content: row.get(1)?,
                    session: row.get(2)?,
                    decay_score: row.get(3)?,
                    access_count: row.get(4)?,
                    trust: row.get(5)?,
                    state: row.get(6)?,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if cands.len() < 2 {
            return Ok(AssociateBatch::default());
        }

        let do_semantic = adapter.reachable() && adapter.embeds_semantically();
        // Semantic pairing is O(n²); bound it to a window. Full-corpus semantic
        // association awaits ANN (M9 HNSW).
        let sem_window = cands.len().min(CO_OCCUR_GROUP_CAP);
        let mut mem_vectors: Vec<Option<Vec<f32>>> = vec![None; cands.len()];
        if do_semantic {
            let texts: Vec<String> = cands
                .iter()
                .take(sem_window)
                .map(|c| c.content.clone())
                .collect();
            if let Ok(vectors) = adapter.embed(&texts) {
                for (i, vector) in vectors.into_iter().enumerate() {
                    if !vector.is_empty() {
                        mem_vectors[i] = Some(vector);
                    }
                }
            }
        }

        let mut batch = AssociateBatch::default();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;

        // Persist memory embeddings (reused by semantic recall rerank).
        if do_semantic {
            for (i, c) in cands.iter().enumerate().take(sem_window) {
                if let Some(vector) = &mem_vectors[i] {
                    Self::store_embedding(
                        &tx,
                        "memory",
                        &c.id,
                        adapter.model_id(),
                        vector,
                        now_ms,
                    )?;
                }
            }
        }

        // Co-occurrence: group by session, cap each group, link every pair (both directions).
        let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
        for (i, c) in cands.iter().enumerate() {
            if let Some(session) = &c.session {
                groups.entry(session.as_str()).or_default().push(i);
            }
        }
        for members in groups.values() {
            let capped = &members[..members.len().min(CO_OCCUR_GROUP_CAP)];
            for a in 0..capped.len() {
                for b in (a + 1)..capped.len() {
                    let ia = capped[a];
                    let ib = capped[b];
                    // Count the logical pair once. A pair can be asymmetric on disk
                    // (fan-out can prune one direction), so consult both inserts.
                    let fwd = upsert_cooccur(&tx, &cands[ia].id, &cands[ib].id, now_ms)?;
                    let rev = upsert_cooccur(&tx, &cands[ib].id, &cands[ia].id, now_ms)?;
                    if fwd || rev {
                        batch.links_created += 1;
                    } else {
                        batch.links_reinforced += 1;
                    }
                }
            }
        }

        // Embedding-similarity links over the bounded window (cross-session allowed).
        if do_semantic {
            for i in 0..sem_window {
                for j in (i + 1)..sem_window {
                    if let (Some(vi), Some(vj)) = (&mem_vectors[i], &mem_vectors[j]) {
                        let cos = f64::from(cosine(vi, vj));
                        if cos >= SEM_LINK_THRESHOLD {
                            let weight = ((cos + 1.0) / 2.0).clamp(0.0, 1.0);
                            let fwd =
                                upsert_semantic(&tx, &cands[i].id, &cands[j].id, weight, now_ms)?;
                            let rev =
                                upsert_semantic(&tx, &cands[j].id, &cands[i].id, weight, now_ms)?;
                            if fwd || rev {
                                batch.links_created += 1;
                            } else {
                                batch.links_reinforced += 1;
                            }
                        }
                    }
                }
            }
        }

        // Prune weak links, then enforce the per-node fan-out cap (bounds growth, §21.10).
        batch.links_pruned += tx.execute(
            "DELETE FROM memory_links WHERE weight < ?1",
            params![WEAK_LINK_FLOOR],
        )?;
        // Fan-out cap is per *node* (across all link types), not per type: partitioning
        // by link_type would let a node hold CAP co-occurrence + CAP semantic edges,
        // violating the §21.10 "<= ASSOCIATE_FANOUT_CAP per node" bound. Strongest edges
        // survive regardless of type (under `null` there are no semantic links, so no
        // cross-type competition occurs).
        batch.links_pruned += tx.execute(
            "DELETE FROM memory_links WHERE id IN (
                 SELECT id FROM (
                     SELECT id, ROW_NUMBER() OVER (
                         PARTITION BY src_memory_id ORDER BY weight DESC, id ASC
                     ) AS rn
                     FROM memory_links
                 ) WHERE rn > ?1)",
            params![ASSOCIATE_FANOUT_CAP as i64],
        )?;

        // Recompute centrality for the batch's candidates over the final graph; fold it
        // into relevance_score; promote `active → associated` when a node has any link.
        for c in &cands {
            let sum: f64 = tx.query_row(
                "SELECT COALESCE(SUM(weight), 0.0) FROM memory_links WHERE src_memory_id = ?1",
                params![c.id],
                |row| row.get(0),
            )?;
            let degree: i64 = tx.query_row(
                "SELECT COUNT(*) FROM memory_links WHERE src_memory_id = ?1",
                params![c.id],
                |row| row.get(0),
            )?;
            let centrality = centrality_for(sum);
            let new_state: &str = if degree > 0 && c.state == "active" {
                "associated"
            } else {
                c.state.as_str()
            };
            let relevance = score_base(
                c.decay_score,
                c.access_count,
                c.trust,
                new_state,
                centrality,
            );
            tx.execute(
                "UPDATE memories SET centrality = ?1, relevance_score = ?2, lifecycle_state = ?3
                 WHERE id = ?4",
                params![centrality, relevance, new_state, c.id],
            )?;
            if degree > 0 {
                batch.nodes_associated += 1;
            }
        }

        let detail = serde_json::to_string(&serde_json::json!({
            "links_created": batch.links_created,
            "links_reinforced": batch.links_reinforced,
            "links_pruned": batch.links_pruned,
            "nodes_associated": batch.nodes_associated,
        }))?;
        insert_audit_log(
            &tx,
            AUDIT_ACTOR,
            "associate",
            "memory",
            None,
            Some(detail.as_str()),
            now_ms,
        )?;
        tx.commit()?;
        Ok(batch)
    }

    /// Propose profile facts from durable, profile-relevant memories into
    /// `approvals(pending)` — **never** writing `profile_facts` directly (H6: the
    /// `profile_facts.approval_id` NOT NULL FK structurally forbids an un-approved fact).
    /// Deterministic, propose-once-per-`fact_key`: a candidate is skipped when any
    /// approval already exists for its key (any state) or an active fact holds it. The
    /// "any state" scope is deliberate — re-proposing a *rejected* fact on every dream
    /// (every ~300s under `serve`) would nag the owner, which violates the
    /// never-surprise-the-user mandate. The trade-off is that a rejection is durable;
    /// re-proposal/un-reject is a deferred enhancement (the rejection is in `audit_log`).
    /// The fact *value* is optionally refined by
    /// the gated LLM `summarize` path (exercised by a metered test-double; the `null`
    /// adapter uses the memory content verbatim — no spend, no network). One IMMEDIATE tx.
    pub(crate) fn extract_profile_pending<A: ProviderAdapter>(
        &mut self,
        adapter: &A,
        budget_usd: f64,
        window_spend: &mut f64,
        limit: usize,
        now_ms: i64,
    ) -> Result<ExtractBatch, StoreError> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        // Profile-relevant kinds: durable owner knowledge (§9.6 long half-lives). Episodic
        // kinds (observation/ephemeral/task) are excluded so approvals are not flooded.
        let cands: Vec<(String, String, String, f64)> = {
            let mut stmt = self.conn.prepare(
                "SELECT id, kind, content, source_trust FROM memories
                 WHERE kind IN ('identity', 'preference', 'fact', 'decision')
                   AND lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')
                 ORDER BY id LIMIT ?1",
            )?;
            let mapped = stmt.query_map(params![limit_i], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, f64>(3)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if cands.is_empty() {
            return Ok(ExtractBatch::default());
        }

        let mut batch = ExtractBatch::default();
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (mem_id, kind, content, trust) in &cands {
            let fact_key = profile_fact_key(kind, content);
            let already_proposed: bool = tx
                .query_row(
                    "SELECT 1 FROM approvals
                     WHERE target_type = 'profile_fact' AND target_ref = ?1 LIMIT 1",
                    params![fact_key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            let already_active: bool = tx
                .query_row(
                    "SELECT 1 FROM profile_facts WHERE fact_key = ?1 AND state = 'active' LIMIT 1",
                    params![fact_key],
                    |_| Ok(()),
                )
                .optional()?
                .is_some();
            if already_proposed || already_active {
                batch.skipped += 1;
                continue;
            }

            // Optional gated LLM refinement of the fact value (mirrors consolidation).
            let price = adapter.usd_per_1k_prompt_tokens();
            let tokens_est = prompt_token_estimate(content);
            let fact_value = if price > 0.0 {
                let est_cost = tokens_est as f64 / 1000.0 * price;
                if *window_spend + est_cost <= budget_usd {
                    match adapter.summarize(std::slice::from_ref(content))? {
                        Some(summary) => {
                            tx.execute(
                                "INSERT INTO provider_usage
                                    (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
                                 VALUES (?1, ?2, ?3, 'complete', ?4, 0, ?5, NULL)",
                                params![now_ms, adapter.id(), adapter.model_id(), tokens_est, est_cost],
                            )?;
                            *window_spend += est_cost;
                            batch.tokens += tokens_est;
                            summary
                        }
                        None => content.clone(),
                    }
                } else {
                    batch.budget_hit = true;
                    content.clone()
                }
            } else {
                adapter
                    .summarize(std::slice::from_ref(content))?
                    .unwrap_or_else(|| content.clone())
            };
            // Defense in depth: memory content was redacted at capture, but a
            // provider summary (or any upstream miss) must not introduce a
            // secret into approvals/audit rows.
            let fact_value = redact_inline_string_with_count(&fact_value).value;

            let proposed_change = serde_json::to_string(&serde_json::json!({
                "fact_key": fact_key,
                "fact_value": fact_value,
                "confidence": trust,
                "source_memory_id": mem_id,
            }))?;
            let approval_id: String =
                tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
            tx.execute(
                "INSERT INTO approvals
                    (id, target_type, target_ref, proposed_change, state, requested_at)
                 VALUES (?1, 'profile_fact', ?2, ?3, 'pending', ?4)",
                params![approval_id, fact_key, proposed_change, now_ms],
            )?;
            insert_audit_log(
                &tx,
                AUDIT_ACTOR,
                "propose_profile_fact",
                "approval",
                Some(approval_id.as_str()),
                Some(proposed_change.as_str()),
                now_ms,
            )?;
            batch.proposed += 1;
        }
        tx.commit()?;
        Ok(batch)
    }

    /// List pending approvals oldest-first (uses the `approvals_pending` index, no scan).
    pub fn list_pending_approvals(&self, limit: usize) -> Result<Vec<ApprovalRow>, StoreError> {
        let limit_i = i64::try_from(limit).unwrap_or(i64::MAX);
        let mut stmt = self.conn.prepare(
            "SELECT id, target_type, target_ref, proposed_change, requested_at
             FROM approvals WHERE state = 'pending'
             ORDER BY requested_at ASC, id ASC LIMIT ?1",
        )?;
        let rows = stmt
            .query_map(params![limit_i], |row| {
                Ok(ApprovalRow {
                    id: row.get(0)?,
                    target_type: row.get(1)?,
                    target_ref: row.get(2)?,
                    proposed_change: row.get(3)?,
                    requested_at: row.get(4)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;
        Ok(rows)
    }

    /// Decide a pending approval. `accept=true` on a `profile_fact` commits it to
    /// `profile_facts` (superseding any active fact for the same key, preserving the
    /// UNIQUE-active invariant + lineage) citing this approval's id (H6). `accept=false`
    /// writes **no** fact. Deciding a non-pending approval is an idempotent no-op.
    /// Unknown id is an error. One IMMEDIATE tx.
    pub fn decide_approval(
        &mut self,
        approval_id: &str,
        accept: bool,
        now_ms: i64,
    ) -> Result<ApprovalDecision, StoreError> {
        let row: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT target_type, proposed_change FROM approvals WHERE id = ?1",
                params![approval_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .optional()?;
        let Some((target_type, proposed_change)) = row else {
            return Err(StoreError::ApprovalNotFound(approval_id.to_string()));
        };

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        // Authoritative state check INSIDE the write tx: the outer read holds no lock, so
        // a second decider (e.g. a concurrent CLI process) could otherwise slip past it
        // and double-commit. The IMMEDIATE tx serializes writers; re-reading here sees
        // any prior committed decision.
        let state: String = tx.query_row(
            "SELECT state FROM approvals WHERE id = ?1",
            params![approval_id],
            |r| r.get(0),
        )?;
        if state != "pending" {
            return Ok(ApprovalDecision {
                state,
                committed_fact: false,
                already_decided: true,
            });
        }
        let mut fact_value_redactions = 0usize;
        let (new_state, committed_fact) = if accept {
            tx.execute(
                "UPDATE approvals SET state = 'approved', decided_at = ?1 WHERE id = ?2",
                params![now_ms, approval_id],
            )?;
            let committed = if target_type == "profile_fact" {
                let change: serde_json::Value = serde_json::from_str(&proposed_change)?;
                let fact_key = change
                    .get("fact_key")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                // Defense in depth: the fact value derives from memory content
                // that was redacted at capture, but anything upstream ever
                // misses must not be re-persisted (and audit-logged) here.
                let RedactedString {
                    value: fact_value,
                    redactions,
                } = redact_inline_string_with_count(
                    change
                        .get("fact_value")
                        .and_then(|v| v.as_str())
                        .unwrap_or(""),
                );
                fact_value_redactions = redactions;
                if fact_key.is_empty() {
                    return Err(StoreError::MalformedApproval(format!(
                        "approval {approval_id} has no fact_key"
                    )));
                }
                let confidence = change
                    .get("confidence")
                    .and_then(serde_json::Value::as_f64)
                    .unwrap_or(0.0);
                let source_memory_id = change.get("source_memory_id").and_then(|v| v.as_str());
                tx.execute(
                    "UPDATE profile_facts SET state = 'superseded'
                     WHERE fact_key = ?1 AND state = 'active'",
                    params![fact_key],
                )?;
                let fact_id: String =
                    tx.query_row("SELECT lower(hex(randomblob(16)))", [], |row| row.get(0))?;
                tx.execute(
                    "INSERT INTO profile_facts
                        (id, fact_key, fact_value, confidence, source_memory_id, approval_id,
                         state, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'active', ?7)",
                    params![
                        fact_id,
                        fact_key,
                        fact_value,
                        confidence,
                        source_memory_id,
                        approval_id,
                        now_ms
                    ],
                )?;
                true
            } else {
                false
            };
            ("approved", committed)
        } else {
            tx.execute(
                "UPDATE approvals SET state = 'rejected', decided_at = ?1 WHERE id = ?2",
                params![now_ms, approval_id],
            )?;
            ("rejected", false)
        };
        let action = if !accept {
            "reject_approval"
        } else if committed_fact {
            "approve_profile_fact"
        } else {
            "approve"
        };
        let detail = if fact_value_redactions > 0 {
            Some(serde_json::to_string(&serde_json::json!({
                "fact_value_redactions": fact_value_redactions,
                "replacement": REDACTED,
            }))?)
        } else {
            None
        };
        insert_audit_log(
            &tx,
            AUDIT_ACTOR,
            action,
            "approval",
            Some(approval_id),
            detail.as_deref(),
            now_ms,
        )?;
        tx.commit()?;
        Ok(ApprovalDecision {
            state: new_state.to_string(),
            committed_fact,
            already_decided: false,
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
            // This is the single interpolated-SQL site in the store and must
            // never take dynamic input: identifiers cannot be bound as
            // parameters, so safety rests on the hardcoded const list.
            debug_assert!(
                CANONICAL_TABLES.contains(&table),
                "table_stats must only interpolate canonical table names"
            );
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
        let hits = self.lexical_hits(&fts_query, limit)?;
        Ok(RecallResult {
            hits,
            degraded: false,
            mode: "lexical",
            compared: 0,
        })
    }

    fn lexical_hits(&self, fts_query: &str, limit: i64) -> Result<Vec<RecallHit>, StoreError> {
        let mut stmt = self.conn.prepare(RECALL_EVENTS_SQL)?;
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
        Ok(hits)
    }

    /// Semantic recall: rerank the bounded FTS shortlist by cosine over cached
    /// `embeddings`, fused with the lexical score. Degrades to lexical (same shape)
    /// when the adapter offers no usable semantic signal or the query embedding
    /// cannot be obtained. Only FTS-prefiltered candidates are ever compared (at
    /// most `RECALL_CANDIDATE_CAP` vectors).
    pub fn recall_semantic(
        &self,
        query: &str,
        limit: usize,
        adapter: &dyn ProviderAdapter,
        index: &dyn VectorIndex,
        now_ms: i64,
    ) -> Result<RecallResult, StoreError> {
        let fts_query = lexical_query(query).ok_or(StoreError::InvalidRecallQuery)?;
        let limit = i64::try_from(limit.clamp(1, 50)).unwrap_or(50);

        if !adapter.reachable() || !adapter.embeds_semantically() {
            let hits = self.lexical_hits(&fts_query, limit)?;
            return Ok(RecallResult {
                hits,
                degraded: true,
                mode: "lexical",
                compared: 0,
            });
        }

        let candidates = self.lexical_hits(&fts_query, RECALL_CANDIDATE_CAP)?;
        if candidates.is_empty() {
            // Semantic was attempted but nothing matched: not a degrade, just empty.
            return Ok(RecallResult {
                hits: Vec::new(),
                degraded: false,
                mode: "semantic",
                compared: 0,
            });
        }

        let query_vec = match self.query_embedding(query, adapter, now_ms)? {
            Some(vector) => vector,
            None => {
                let hits = self.lexical_hits(&fts_query, limit)?;
                return Ok(RecallResult {
                    hits,
                    degraded: true,
                    mode: "lexical",
                    compared: 0,
                });
            }
        };

        let model_id = adapter.model_id();
        let mut vectors: Vec<Candidate> = Vec::with_capacity(candidates.len());
        for hit in &candidates {
            if let Some(vector) =
                self.embedding_vector("raw_event", &hit.raw_event_id.to_string(), model_id)?
            {
                vectors.push(Candidate {
                    id: hit.raw_event_id,
                    vector,
                });
            }
        }
        let compared = vectors.len();
        if compared == 0 {
            // Provider worked but no shortlisted event is embedded yet: fall back to
            // lexical, flagged degraded like the other no-semantic-signal paths.
            let hits = self.lexical_hits(&fts_query, limit)?;
            return Ok(RecallResult {
                hits,
                degraded: true,
                mode: "lexical",
                compared: 0,
            });
        }

        // Score every candidate (k = all): fusion needs each candidate's cosine, not
        // an ANN top-k. (M9 HNSW will revisit this integration.)
        let scored = index.search(&query_vec, &vectors, vectors.len());
        let cosine: HashMap<i64, f32> = scored.into_iter().map(|s| (s.id, s.score)).collect();

        // Lexical term: -bm25 (more negative bm25 = better match) min-max normalized.
        let lex_raw: Vec<f32> = candidates.iter().map(|hit| -(hit.score as f32)).collect();
        let (lo, hi) = lex_raw
            .iter()
            .fold((f32::INFINITY, f32::NEG_INFINITY), |(lo, hi), &v| {
                (lo.min(v), hi.max(v))
            });
        let span = hi - lo;
        // Plan s9.4 weights cover a 7-term formula; M4 uses only the two active
        // signals, so renormalize them to sum to 1.0 (fused score stays in [0, 1]).
        let weight_sum = RECALL_W_SEM + RECALL_W_LEX;
        let w_sem = RECALL_W_SEM / weight_sum;
        let w_lex = RECALL_W_LEX / weight_sum;

        let mut fused: Vec<RecallHit> = candidates
            .into_iter()
            .enumerate()
            .map(|(i, mut hit)| {
                let lex = if span > 0.0 {
                    (lex_raw[i] - lo) / span
                } else {
                    0.5
                };
                let sem = cosine
                    .get(&hit.raw_event_id)
                    .map_or(0.0, |c| (c + 1.0) / 2.0);
                hit.score = f64::from(w_sem * sem + w_lex * lex);
                hit
            })
            .collect();
        fused.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.ts_ms.cmp(&a.ts_ms))
                .then(b.raw_event_id.cmp(&a.raw_event_id))
        });
        fused.truncate(limit as usize);

        Ok(RecallResult {
            hits: fused,
            degraded: false,
            mode: "semantic",
            compared,
        })
    }

    /// Durable-memory recall with optional one-hop graph expansion (ARCHITECTURE-PLAN
    /// §5.3 path (c)/(d), §9.3 `R_recall`, §21.10). Pipeline: FTS5 prefilter over
    /// `memories_fts` → optional semantic rerank (cosine over cached memory embeddings,
    /// only when the adapter carries a usable semantic signal) → optional one-hop
    /// expansion over `memory_links` (`hops == 1`) → fuse the canonical recall variables
    /// → top-`limit`. Returns an empty result (no error) when nothing matches, so the
    /// caller can fall back to raw-event recall. Reads only (aside from the optional
    /// query-embedding cache write, shared with [`Store::recall_semantic`]).
    pub fn recall_memories(
        &self,
        query: &str,
        limit: usize,
        hops: u8,
        adapter: &dyn ProviderAdapter,
        now_ms: i64,
    ) -> Result<MemoryRecallResult, StoreError> {
        struct Cand {
            id: String,
            kind: String,
            content: String,
            bm25: Option<f64>,
            centrality: f64,
            trust: f64,
            last_accessed: i64,
            access_count: i64,
            state: String,
            link_strength: f64,
            via_hop: bool,
        }
        let fts_query = lexical_query(query).ok_or(StoreError::InvalidRecallQuery)?;
        let limit = limit.clamp(1, 50);
        let mode: &'static str = if hops >= 1 { "memory+graph" } else { "memory" };

        // (a) FTS5 prefilter, bounded to RECALL_CANDIDATE_CAP best lexical matches.
        let mut cands: Vec<Cand> = {
            let mut stmt = self.conn.prepare(
                "SELECT f.memory_id, m.kind, m.content, bm25(memories_fts) AS score,
                        m.centrality, m.source_trust,
                        COALESCE(m.last_accessed_at, m.created_at), m.access_count,
                        m.lifecycle_state
                 FROM memories_fts f JOIN memories m ON m.id = f.memory_id
                 WHERE memories_fts MATCH ?1
                   AND m.lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')
                 ORDER BY score LIMIT ?2",
            )?;
            let mapped = stmt.query_map(params![fts_query, RECALL_CANDIDATE_CAP], |row| {
                Ok(Cand {
                    id: row.get(0)?,
                    kind: row.get(1)?,
                    content: row.get(2)?,
                    bm25: Some(row.get(3)?),
                    centrality: row.get(4)?,
                    trust: row.get(5)?,
                    last_accessed: row.get(6)?,
                    access_count: row.get(7)?,
                    state: row.get(8)?,
                    link_strength: 0.0,
                    via_hop: false,
                })
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        if cands.is_empty() {
            return Ok(MemoryRecallResult {
                hits: Vec::new(),
                degraded: false,
                mode,
                compared: 0,
            });
        }

        // (b) Optional semantic rerank: cosine of the query embedding against cached
        // per-memory embeddings. Only attempted when the adapter has a usable signal.
        let wants_semantic = adapter.reachable() && adapter.embeds_semantically();
        let mut cosines: HashMap<String, f64> = HashMap::new();
        let mut compared = 0usize;
        if wants_semantic && let Some(query_vec) = self.query_embedding(query, adapter, now_ms)? {
            let model_id = adapter.model_id();
            for c in &cands {
                if let Some(vector) = self.embedding_vector("memory", &c.id, model_id)? {
                    cosines.insert(c.id.clone(), f64::from(cosine(&query_vec, &vector)));
                    compared += 1;
                }
            }
        }
        let semantic_available = compared > 0;
        let degraded = wants_semantic && !semantic_available;

        // (c) One-hop expansion: pull strongest neighbors of each direct hit (links are
        // stored symmetrically, so a single src lookup yields the full neighborhood).
        if hops >= 1 {
            let direct_ids: Vec<String> = cands.iter().map(|c| c.id.clone()).collect();
            let mut seen: std::collections::HashMap<String, usize> = cands
                .iter()
                .enumerate()
                .map(|(i, c)| (c.id.clone(), i))
                .collect();
            // Prepared once and reused across all direct hits (recall is on the hot path):
            // one statement for the adjacency lookup, one for fetching a neighbor's row.
            let mut neighbor_stmt = self.conn.prepare(
                "SELECT dst_memory_id, weight FROM memory_links
                 WHERE src_memory_id = ?1 ORDER BY weight DESC, dst_memory_id LIMIT ?2",
            )?;
            let mut neighbor_row_stmt = self.conn.prepare(
                "SELECT kind, content, centrality, source_trust,
                        COALESCE(last_accessed_at, created_at), access_count, lifecycle_state
                 FROM memories
                 WHERE id = ?1
                   AND lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')",
            )?;
            for src in &direct_ids {
                let neighbors: Vec<(String, f64)> = neighbor_stmt
                    .query_map(params![src, ASSOCIATE_FANOUT_CAP as i64], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, f64>(1)?))
                    })?
                    .collect::<Result<Vec<_>, _>>()?;
                for (dst, weight) in neighbors {
                    if let Some(&idx) = seen.get(&dst) {
                        // Already a candidate: it also earns the strongest reaching link.
                        if weight > cands[idx].link_strength {
                            cands[idx].link_strength = weight;
                        }
                        continue;
                    }
                    let row: Option<(String, String, f64, f64, i64, i64, String)> =
                        neighbor_row_stmt
                            .query_row(params![dst], |r| {
                                Ok((
                                    r.get(0)?,
                                    r.get(1)?,
                                    r.get(2)?,
                                    r.get(3)?,
                                    r.get(4)?,
                                    r.get(5)?,
                                    r.get(6)?,
                                ))
                            })
                            .optional()?;
                    if let Some((
                        kind,
                        content,
                        centrality,
                        trust,
                        last_accessed,
                        access_count,
                        state,
                    )) = row
                    {
                        seen.insert(dst.clone(), cands.len());
                        cands.push(Cand {
                            id: dst,
                            kind,
                            content,
                            bm25: None,
                            centrality,
                            trust,
                            last_accessed,
                            access_count,
                            state,
                            link_strength: weight,
                            via_hop: true,
                        });
                    }
                }
            }
        }

        // (d) Fuse the canonical recall variables. Lexical and centrality are min-max
        // normalized within the candidate set (§9.3/§9.4).
        let lex_raw: Vec<f64> = cands.iter().map(|c| c.bm25.map_or(0.0, |b| -b)).collect();
        let (lex_lo, lex_hi) = cands
            .iter()
            .enumerate()
            .filter(|(_, c)| c.bm25.is_some())
            .map(|(i, _)| lex_raw[i])
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(v), hi.max(v))
            });
        let lex_span = lex_hi - lex_lo;
        let (cen_lo, cen_hi) = cands
            .iter()
            .map(|c| c.centrality)
            .fold((f64::INFINITY, f64::NEG_INFINITY), |(lo, hi), v| {
                (lo.min(v), hi.max(v))
            });
        let cen_span = cen_hi - cen_lo;

        let mut hits: Vec<MemoryHit> = cands
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let lexical = if c.bm25.is_none() {
                    0.0
                } else if lex_span > 0.0 {
                    (lex_raw[i] - lex_lo) / lex_span
                } else {
                    0.5
                };
                let centrality_norm = if cen_span > 0.0 {
                    (c.centrality - cen_lo) / cen_span
                } else {
                    0.0
                };
                let semantic = cosines.get(&c.id).map_or(0.0, |cos| (cos + 1.0) / 2.0);
                let terms = crate::dream::RecallTerms {
                    semantic,
                    lexical,
                    recency: crate::dream::recency_term(now_ms - c.last_accessed),
                    access_freq: crate::dream::access_frequency(c.access_count),
                    link_strength: c.link_strength,
                    centrality: centrality_norm,
                    source_trust: c.trust,
                    lifecycle_bonus: crate::dream::lifecycle_bonus_signed(&c.state),
                };
                MemoryHit {
                    memory_id: c.id.clone(),
                    kind: c.kind.clone(),
                    content: c.content.clone(),
                    score: crate::dream::score_recall(&terms, semantic_available),
                    via_hop: c.via_hop,
                    link_strength: c.link_strength,
                }
            })
            .collect();
        // Deterministic order: score desc, then memory_id desc (last_accessed is folded
        // into the score via the recency term).
        hits.sort_by(|a, b| {
            b.score
                .partial_cmp(&a.score)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then(b.memory_id.cmp(&a.memory_id))
        });
        hits.truncate(limit);

        Ok(MemoryRecallResult {
            hits,
            degraded,
            mode,
            compared,
        })
    }

    /// One-hop association walk from a single memory (the MCP `memory_graph` tool).
    /// Links are stored symmetrically, so a single src lookup yields the full
    /// neighborhood. Returns `None` when the id is unknown or not in a recallable
    /// lifecycle state; neighbors in non-recallable states are filtered out.
    pub fn memory_neighbors(
        &self,
        memory_id: &str,
        limit: usize,
    ) -> Result<Option<MemoryNeighborhood>, StoreError> {
        let limit = i64::try_from(limit.clamp(1, 50)).unwrap_or(1);
        let source: Option<(String, String)> = self
            .conn
            .query_row(
                "SELECT kind, content FROM memories
                 WHERE id = ?1
                   AND lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')",
                params![memory_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((kind, content)) = source else {
            return Ok(None);
        };

        let mut stmt = self.conn.prepare(
            "SELECT l.dst_memory_id, m.kind, m.content, l.link_type, l.weight,
                    l.last_reinforced_at, m.lifecycle_state
             FROM memory_links l JOIN memories m ON m.id = l.dst_memory_id
             WHERE l.src_memory_id = ?1
               AND m.lifecycle_state IN ('active', 'associated', 'decaying', 'dormant')
             ORDER BY l.weight DESC, l.dst_memory_id LIMIT ?2",
        )?;
        let neighbors = stmt
            .query_map(params![memory_id, limit], |row| {
                Ok(MemoryNeighbor {
                    memory_id: row.get(0)?,
                    kind: row.get(1)?,
                    content: row.get(2)?,
                    link_type: row.get(3)?,
                    link_strength: row.get(4)?,
                    last_reinforced_at: row.get(5)?,
                    lifecycle_state: row.get(6)?,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?;

        Ok(Some(MemoryNeighborhood {
            memory_id: memory_id.to_string(),
            kind,
            content,
            neighbors,
        }))
    }

    /// Fetch or compute+cache the query embedding. `None` => provider returned no
    /// usable vector (caller degrades to lexical). A cache hit makes no provider
    /// call and writes no ledger row.
    fn query_embedding(
        &self,
        query: &str,
        adapter: &dyn ProviderAdapter,
        now_ms: i64,
    ) -> Result<Option<Vec<f32>>, StoreError> {
        let model_id = adapter.model_id();
        let owner_id = query_cache_id(query);
        if let Some(vector) = self.embedding_vector("query", &owner_id, model_id)? {
            return Ok(Some(vector));
        }
        // embed_query (not embed): asymmetric-retrieval adapters prefix queries.
        let Some(vector) = adapter.embed_query(query).ok().filter(|v| !v.is_empty()) else {
            return Ok(None);
        };
        // Cache the embedding and its usage ledger row atomically (unchecked_transaction
        // is the &self escape hatch; no outer transaction is held here).
        let tx = self.conn.unchecked_transaction()?;
        Self::store_embedding(&tx, "query", &owner_id, model_id, &vector, now_ms)?;
        tx.execute(
            "INSERT INTO provider_usage
                (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
             VALUES (?1, ?2, ?3, 'embed', ?4, 0, 0.0, NULL)",
            params![now_ms, adapter.id(), model_id, prompt_token_estimate(query)],
        )?;
        tx.commit()?;
        Ok(Some(vector))
    }

    fn embedding_vector(
        &self,
        owner_type: &str,
        owner_id: &str,
        model_id: &str,
    ) -> Result<Option<Vec<f32>>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .conn
            .query_row(
                "SELECT vector FROM embeddings
                 WHERE owner_type = ?1 AND owner_id = ?2 AND model_id = ?3",
                params![owner_type, owner_id, model_id],
                |row| row.get(0),
            )
            .optional()?;
        Ok(bytes.map(|bytes| decode_f32(&bytes)))
    }

    fn store_embedding(
        conn: &Connection,
        owner_type: &str,
        owner_id: &str,
        model_id: &str,
        vector: &[f32],
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let dim = i64::try_from(vector.len()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(vector.len() * 4);
        for value in vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        conn.execute(
            "INSERT INTO embeddings (id, owner_type, owner_id, model_id, dim, vector, created_at)
             VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(owner_type, owner_id, model_id)
             DO UPDATE SET dim = excluded.dim, vector = excluded.vector,
                           created_at = excluded.created_at",
            params![owner_type, owner_id, model_id, dim, bytes, now_ms],
        )?;
        Ok(())
    }

    /// Atomically claim up to `limit` ready embed jobs.
    ///
    /// "Ready" = `pending`/`deferred` whose `scheduled_at` has arrived, OR `running`
    /// jobs whose lease expired (`started_at <= now - visibility_ms`). SQLite serializes
    /// writers, so two workers can never claim the same row.
    ///
    /// Priority then `scheduled_at` (then `id`) governs which jobs are selected; the
    /// returned batch itself is in `id` order, not priority order (`UPDATE ... RETURNING`
    /// does not preserve the subquery ordering). The worker processes the whole batch,
    /// so intra-batch order does not matter.
    pub fn lease_embed_jobs(
        &mut self,
        limit: usize,
        now_ms: i64,
        visibility_ms: i64,
    ) -> Result<Vec<LeasedJob>, StoreError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let claimed: Vec<(i64, String, i64)> = {
            let mut stmt = tx.prepare(
                "UPDATE jobs SET state = 'running', started_at = ?1, attempts = attempts + 1
                 WHERE id IN (
                     SELECT id FROM jobs
                     WHERE kind = 'embed' AND (
                         (state IN ('pending', 'deferred') AND scheduled_at <= ?1)
                      OR (state = 'running' AND started_at IS NOT NULL
                          AND started_at <= ?1 - ?2))
                     ORDER BY priority, scheduled_at, id
                     LIMIT ?3)
                 RETURNING id, payload, attempts",
            )?;
            stmt.query_map(params![now_ms, visibility_ms, limit], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?
        };

        let mut leased = Vec::with_capacity(claimed.len());
        for (job_id, payload, attempts) in claimed {
            let payload: serde_json::Value = serde_json::from_str(&payload)?;
            let raw_event_id = payload
                .get("raw_event_id")
                .and_then(serde_json::Value::as_i64)
                .unwrap_or(0);
            let content: String = tx
                .query_row(
                    "SELECT content FROM raw_events_fts WHERE raw_event_id = ?1",
                    params![raw_event_id],
                    |row| row.get(0),
                )
                .optional()?
                .unwrap_or_default();
            leased.push(LeasedJob {
                job_id,
                raw_event_id,
                content,
                attempts,
            });
        }
        tx.commit()?;
        Ok(leased)
    }

    /// Persist an embedding, its `provider_usage` ledger row, and mark the job done —
    /// all in one transaction so a crash never leaves a done job without its embedding.
    ///
    /// Precondition: invoked by the worker holding the current lease. The `embeddings`
    /// write is an idempotent upsert, so a reclaimed (re-processed) job is harmless;
    /// full lease-epoch fencing is deferred with the remote adapters.
    pub fn complete_embed_job(
        &mut self,
        job_id: i64,
        raw_event_id: i64,
        provider: EmbedProvider<'_>,
        vector: &[f32],
        prompt_tokens: i64,
        now_ms: i64,
    ) -> Result<(), StoreError> {
        let EmbedProvider {
            adapter_id,
            model_id,
        } = provider;
        let dim = i64::try_from(vector.len()).unwrap_or(0);
        let mut bytes = Vec::with_capacity(vector.len() * 4);
        for value in vector {
            bytes.extend_from_slice(&value.to_le_bytes());
        }
        let owner_id = raw_event_id.to_string();

        let tx = self
            .conn
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        tx.execute(
            "INSERT INTO embeddings (id, owner_type, owner_id, model_id, dim, vector, created_at)
             VALUES (lower(hex(randomblob(16))), 'raw_event', ?1, ?2, ?3, ?4, ?5)
             ON CONFLICT(owner_type, owner_id, model_id)
             DO UPDATE SET dim = excluded.dim, vector = excluded.vector,
                           created_at = excluded.created_at",
            params![owner_id, model_id, dim, bytes, now_ms],
        )?;
        tx.execute(
            "INSERT INTO provider_usage
                (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
             VALUES (?1, ?2, ?3, 'embed', ?4, 0, 0.0, ?5)",
            params![now_ms, adapter_id, model_id, prompt_tokens, job_id],
        )?;
        tx.execute(
            "UPDATE jobs SET state = 'done', finished_at = ?2 WHERE id = ?1",
            params![job_id, now_ms],
        )?;
        tx.commit()?;
        Ok(())
    }

    /// Defer a failed job with exponential backoff, or dead-letter it once it has
    /// reached `max_attempts`. `attempts` is the job's post-lease attempt count.
    ///
    /// Precondition: the caller holds the current lease (the worker invokes this right
    /// after a failed embed, while the job is `running`). Fencing a stale caller whose
    /// lease already expired and was reclaimed needs a lease epoch; that is deferred
    /// until adapters that can outlast the visibility window (the remote providers) land.
    pub fn fail_job(
        &mut self,
        job_id: i64,
        attempts: i64,
        error: &str,
        now_ms: i64,
        max_attempts: u32,
        backoff_base_ms: u64,
    ) -> Result<JobOutcome, StoreError> {
        if attempts >= i64::from(max_attempts) {
            self.conn.execute(
                "UPDATE jobs SET state = 'dead', finished_at = ?2, last_error = ?3 WHERE id = ?1",
                params![job_id, now_ms, error],
            )?;
            Ok(JobOutcome::Dead)
        } else {
            let exponent = attempts.saturating_sub(1).clamp(0, 16) as u32;
            let backoff = (backoff_base_ms as i64).saturating_mul(1i64 << exponent);
            let scheduled_at = now_ms.saturating_add(backoff);
            self.conn.execute(
                "UPDATE jobs SET state = 'deferred', scheduled_at = ?2, last_error = ?3
                 WHERE id = ?1",
                params![job_id, scheduled_at, error],
            )?;
            Ok(JobOutcome::Deferred { scheduled_at })
        }
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
    /// Number of candidate vectors compared (semantic recall); 0 for lexical.
    pub compared: usize,
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

/// One ranked durable memory returned by [`Store::recall_memories`].
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryHit {
    pub memory_id: String,
    pub kind: String,
    pub content: String,
    pub score: f64,
    /// True when this memory was reached only via a one-hop graph expansion.
    pub via_hop: bool,
    /// Strength of the link a hop-neighbor was reached by (0 for direct hits).
    pub link_strength: f64,
}

/// Result of [`Store::recall_memories`] — the durable-memory + graph recall path.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryRecallResult {
    pub hits: Vec<MemoryHit>,
    pub degraded: bool,
    pub mode: &'static str,
    /// Number of candidate memory vectors compared (semantic rerank); 0 when lexical.
    pub compared: usize,
}

/// One linked memory returned by [`Store::memory_neighbors`].
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryNeighbor {
    pub memory_id: String,
    pub kind: String,
    pub content: String,
    pub link_type: String,
    /// The stored link `weight` (symmetric, reinforced over dream runs).
    pub link_strength: f64,
    pub last_reinforced_at: i64,
    pub lifecycle_state: String,
}

/// Result of [`Store::memory_neighbors`] — a memory plus its one-hop neighborhood.
#[derive(Debug, Clone, PartialEq)]
pub struct MemoryNeighborhood {
    pub memory_id: String,
    pub kind: String,
    pub content: String,
    pub neighbors: Vec<MemoryNeighbor>,
}

/// A claimed embed job — the unit a worker processes for one lease.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LeasedJob {
    pub job_id: i64,
    pub raw_event_id: i64,
    pub content: String,
    pub attempts: i64,
}

/// Outcome of failing a job: deferred with backoff, or dead-lettered.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum JobOutcome {
    Deferred { scheduled_at: i64 },
    Dead,
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
    Import(String),
    Adapter(String),
    ApprovalNotFound(String),
    MalformedApproval(String),
    WriterGone,
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
            Self::Import(msg) => write!(f, "import error: {msg}"),
            Self::Adapter(msg) => write!(f, "provider adapter error: {msg}"),
            Self::ApprovalNotFound(id) => write!(f, "approval not found: {id}"),
            Self::MalformedApproval(msg) => write!(f, "malformed approval: {msg}"),
            Self::WriterGone => write!(f, "store writer thread is gone"),
        }
    }
}

impl std::error::Error for StoreError {}

impl From<ImportError> for StoreError {
    fn from(err: ImportError) -> Self {
        Self::Import(err.to_string())
    }
}

impl From<AdapterError> for StoreError {
    fn from(err: AdapterError) -> Self {
        Self::Adapter(err.to_string())
    }
}

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

/// Known limitation (decision recorded): secrets that arrive percent-encoded
/// are split at `%xx` boundaries by the span collectors and are not reassembled
/// or decoded before matching; decoding arbitrary text risks false positives
/// and double-decode bugs for marginal gain on a local-first daemon. See the
/// README "Security Defaults" known-limitations note.
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
        // Case-insensitive: providers emit canonical casing, but secrets pasted
        // through shells/editors can arrive case-mangled and must still match.
        while let Some(relative) = find_ascii_case_insensitive(&input[offset..], prefix) {
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
        return;
    }
    // All-digit secrets (card numbers, numeric tokens) carry too little
    // Shannon entropy per character to clear the mixed-content gate above, so
    // they get a dedicated length-only rule. Dash/space-grouped digit runs are
    // split by the tokenizer and stay out of scope; ~19-digit nanosecond
    // timestamps will be redacted, which is the acceptable direction of error
    // for a redactor.
    if candidate.len() >= DIGIT_SECRET_MIN_LEN
        && candidate.bytes().all(|byte| byte.is_ascii_digit())
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

/// SQL for provider-free lexical recall over the `raw_events_fts` virtual table.
/// Shared verbatim by [`Store::recall_events`] and the query-plan regression test
/// so the `EXPLAIN QUERY PLAN` assertion can never drift from the query that runs.
const RECALL_EVENTS_SQL: &str = "SELECT r.id, r.session_id, r.ts, r.source, r.kind, f.content,
            bm25(raw_events_fts) AS score
     FROM raw_events_fts AS f
     JOIN raw_events AS r ON r.id = f.raw_event_id
     WHERE raw_events_fts MATCH ?1
     ORDER BY score, r.ts DESC
     LIMIT ?2";

/// Hard cap on vectors compared per semantic recall (H7: never the whole table).
const RECALL_CANDIDATE_CAP: i64 = 256;
/// Recall fusion weights (ARCHITECTURE-PLAN s9.4 defaults; M4 uses the two active
/// signals — semantic + lexical — since durable-memory signals arrive in M6+).
const RECALL_W_SEM: f32 = 0.34;
const RECALL_W_LEX: f32 = 0.18;

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

fn decode_f32(bytes: &[u8]) -> Vec<f32> {
    bytes
        .chunks_exact(4)
        .map(|chunk| f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]))
        .collect()
}

fn query_cache_id(query: &str) -> String {
    // FNV-1a 64-bit: stable across Rust versions/platforms (unlike DefaultHasher), so
    // cached query embeddings keep matching after a toolchain upgrade. Cache key only.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in query.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100_0000_01b3);
    }
    format!("q-{hash:016x}")
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

    let applied_0003 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [3],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0003 {
        tx.execute_batch(MIGRATION_0003)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (3, "0003_raw_event_content_hash"),
        )?;
    }

    let applied_0004 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [4],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0004 {
        tx.execute_batch(MIGRATION_0004)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (4, "0004_memory_decay_and_consolidation"),
        )?;
    }

    let applied_0005 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [5],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0005 {
        tx.execute_batch(MIGRATION_0005)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (5, "0005_association_graph"),
        )?;
    }

    let applied_0006 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [6],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0006 {
        tx.execute_batch(MIGRATION_0006)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (6, "0006_approvals_target_index"),
        )?;
    }

    let applied_0007 = tx
        .query_row(
            "SELECT 1 FROM schema_migrations WHERE version = ?1",
            [7],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();

    if !applied_0007 {
        tx.execute_batch(MIGRATION_0007)?;
        tx.execute(
            "INSERT INTO schema_migrations (version, name, applied_at)
             VALUES (?1, ?2, CAST(strftime('%s', 'now') AS INTEGER) * 1000)",
            (7, "0007_provider_usage_local_adapter"),
        )?;
    }

    tx.commit()?;
    Ok(())
}

/// Provider identity recorded with a completed embed job (ledger + embeddings row).
#[derive(Debug, Clone, Copy)]
pub struct EmbedProvider<'a> {
    pub adapter_id: &'a str,
    pub model_id: &'a str,
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

/// Cap on the whole-file read in [`Store::import_jsonl`] - bounds memory on a small VM.
/// 64 MiB of JSONL is hundreds of thousands of records, well beyond personal scale.
const MAX_IMPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Outcome of staging one import unit: a new row, a dedup skip, or a queue-full pause.
enum StageOutcome {
    Staged,
    Skipped,
    Paused,
}

/// Counts from one consolidation batch (one transaction over a bounded raw_event slice).
pub struct ConsolidateBatch {
    pub memories_created: usize,
    pub raw_consumed: usize,
    pub tokens: i64,
    pub budget_hit: bool,
}

/// Counts from one decay batch.
pub struct DecayBatch {
    pub touched: usize,
}

/// Outcome of one [`Store::associate_pending`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct AssociateBatch {
    /// New link pairs created this run.
    pub links_created: usize,
    /// Existing link pairs reinforced this run.
    pub links_reinforced: usize,
    /// Directed link rows deleted (weak-floor + fan-out cap).
    pub links_pruned: usize,
    /// Memories that hold at least one link after this run.
    pub nodes_associated: usize,
}

/// Outcome of one [`Store::extract_profile_pending`] pass.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ExtractBatch {
    /// Profile-fact approvals newly proposed (pending) this run.
    pub proposed: usize,
    /// Candidates skipped because an approval/active fact already covers the key.
    pub skipped: usize,
    /// Prompt tokens consumed by the gated LLM fact-value refinement.
    pub tokens: i64,
    /// True if the spend cap forced a candidate to fall back to verbatim content.
    pub budget_hit: bool,
}

/// One pending approval, as surfaced by `approve --list`.
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalRow {
    pub id: String,
    pub target_type: String,
    pub target_ref: Option<String>,
    pub proposed_change: String,
    pub requested_at: i64,
}

/// Result of deciding an approval ([`Store::decide_approval`]).
#[derive(Debug, Clone, PartialEq)]
pub struct ApprovalDecision {
    /// Resulting approval state (`approved`/`rejected`, or the existing state if already decided).
    pub state: String,
    /// Whether a `profile_facts` row was committed (accept of a `profile_fact`).
    pub committed_fact: bool,
    /// True if the approval had already been decided (no-op).
    pub already_decided: bool,
}

/// A dedup-cluster of raw_events sharing the same normalized text.
struct Cluster {
    text: String,
    source: String,
    kind: String,
    session: String,
    ids: Vec<i64>,
}

/// Extract the consolidatable text from a raw_event payload (`text` field, else the
/// raw payload JSON).
fn raw_event_text(payload: &str) -> String {
    serde_json::from_str::<serde_json::Value>(payload)
        .ok()
        .and_then(|value| {
            value
                .get("text")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string)
        })
        .unwrap_or_else(|| payload.to_string())
}

/// Deterministic profile-fact key: `{kind}:{slug}`, slug = lowercased,
/// whitespace-collapsed content joined by `-`, truncated to 48 chars. Stable across
/// runs so the propose-once-per-key idempotency check is exact.
fn profile_fact_key(kind: &str, content: &str) -> String {
    let slug: String = normalize(content)
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(48)
        .collect();
    format!("{kind}:{slug}")
}

/// Cosine similarity of two vectors; 0 for a zero-norm vector. Compares the shared
/// prefix when lengths differ (defensive — embeddings of one model share a dim).
fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let n = a.len().min(b.len());
    let (mut dot, mut na, mut nb) = (0.0f32, 0.0f32, 0.0f32);
    for i in 0..n {
        dot += a[i] * b[i];
        na += a[i] * a[i];
        nb += b[i] * b[i];
    }
    if na == 0.0 || nb == 0.0 {
        return 0.0;
    }
    dot / (na.sqrt() * nb.sqrt())
}

/// Insert (weight = `CO_OCCUR_BASE`) or reinforce (weight += `CO_OCCUR_REINFORCE`,
/// capped at 1.0) one directed co-occurrence edge. Returns `true` on first creation.
fn upsert_cooccur(
    tx: &rusqlite::Transaction<'_>,
    src: &str,
    dst: &str,
    now_ms: i64,
) -> Result<bool, StoreError> {
    let existing: Option<f64> = tx
        .query_row(
            "SELECT weight FROM memory_links
             WHERE src_memory_id = ?1 AND dst_memory_id = ?2 AND link_type = 'co_occurrence'",
            params![src, dst],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        None => {
            tx.execute(
                "INSERT INTO memory_links
                    (id, src_memory_id, dst_memory_id, link_type, weight, last_reinforced_at)
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, 'co_occurrence', ?3, ?4)",
                params![src, dst, CO_OCCUR_BASE, now_ms],
            )?;
            Ok(true)
        }
        Some(weight) => {
            let reinforced = (weight + CO_OCCUR_REINFORCE).min(1.0);
            tx.execute(
                "UPDATE memory_links SET weight = ?1, last_reinforced_at = ?2
                 WHERE src_memory_id = ?3 AND dst_memory_id = ?4 AND link_type = 'co_occurrence'",
                params![reinforced, now_ms, src, dst],
            )?;
            Ok(false)
        }
    }
}

/// Insert or strengthen one directed `semantic` edge to `max(existing, weight)`.
/// Returns `true` on first creation.
fn upsert_semantic(
    tx: &rusqlite::Transaction<'_>,
    src: &str,
    dst: &str,
    weight: f64,
    now_ms: i64,
) -> Result<bool, StoreError> {
    let existing: Option<f64> = tx
        .query_row(
            "SELECT weight FROM memory_links
             WHERE src_memory_id = ?1 AND dst_memory_id = ?2 AND link_type = 'semantic'",
            params![src, dst],
            |row| row.get(0),
        )
        .optional()?;
    match existing {
        None => {
            tx.execute(
                "INSERT INTO memory_links
                    (id, src_memory_id, dst_memory_id, link_type, weight, last_reinforced_at)
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, 'semantic', ?3, ?4)",
                params![src, dst, weight, now_ms],
            )?;
            Ok(true)
        }
        Some(old) => {
            tx.execute(
                "UPDATE memory_links SET weight = ?1, last_reinforced_at = ?2
                 WHERE src_memory_id = ?3 AND dst_memory_id = ?4 AND link_type = 'semantic'",
                params![old.max(weight), now_ms, src, dst],
            )?;
            Ok(false)
        }
    }
}

// Staging-dedup key for imported rows (ARCHITECTURE-PLAN s11.6). NULL for native
// captures; the partial unique index makes re-import a no-op for seen content.
const MIGRATION_0003: &str = r#"
ALTER TABLE raw_events ADD COLUMN content_hash BLOB;
CREATE UNIQUE INDEX ux_raw_import_hash ON raw_events(content_hash) WHERE kind = 'import';
"#;

// M6 decay/consolidation columns (ARCHITECTURE-PLAN s9.1) + the consolidation cursor.
const MIGRATION_0004: &str = r#"
ALTER TABLE memories ADD COLUMN source_trust REAL NOT NULL DEFAULT 0.5;
ALTER TABLE memories ADD COLUMN decay_score REAL NOT NULL DEFAULT 1.0;
ALTER TABLE memories ADD COLUMN decay_recomputed_at INTEGER NOT NULL DEFAULT 0;
ALTER TABLE raw_events ADD COLUMN consolidated_at INTEGER;
CREATE INDEX raw_events_unconsolidated ON raw_events(id) WHERE consolidated_at IS NULL;
"#;

const MIGRATION_0005: &str = r#"
ALTER TABLE memories ADD COLUMN centrality REAL NOT NULL DEFAULT 0.0;
ALTER TABLE memories ADD COLUMN source_session TEXT;
CREATE INDEX memories_session ON memories(source_session) WHERE source_session IS NOT NULL;
"#;

const MIGRATION_0006: &str = r#"
CREATE INDEX approvals_target ON approvals(target_type, target_ref);
"#;

// Admit the in-process 'local' adapter into the provider_usage ledger. SQLite cannot
// alter a CHECK constraint, so the table is rebuilt in place (same columns, same
// rowids, same index) with the widened adapter vocabulary.
const MIGRATION_0007: &str = r#"
CREATE TABLE provider_usage_new (
    id INTEGER PRIMARY KEY,
    ts INTEGER NOT NULL,
    adapter TEXT NOT NULL CHECK (adapter IN ('openai_compat', 'ollama', 'opencode', 'null', 'local')),
    model_id TEXT NOT NULL,
    op TEXT NOT NULL CHECK (op IN ('embed', 'complete', 'embed:dry', 'complete:dry')),
    prompt_tokens INTEGER NOT NULL DEFAULT 0,
    completion_tokens INTEGER NOT NULL DEFAULT 0,
    est_cost REAL NOT NULL DEFAULT 0.0,
    job_id INTEGER REFERENCES jobs(id) ON DELETE SET NULL
) STRICT;
INSERT INTO provider_usage_new
    SELECT id, ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id
    FROM provider_usage;
DROP TABLE provider_usage;
ALTER TABLE provider_usage_new RENAME TO provider_usage;
CREATE INDEX provider_usage_ts_adapter ON provider_usage(ts, adapter);
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

    #[test]
    fn recall_events_query_plan_uses_raw_events_fts() {
        let path = temp_db_path("recall-query-plan");
        let store = Store::open(&path).expect("store opens");
        let fts_query = lexical_query("wal busy").expect("query has searchable text");

        // Explain the *production* recall SQL (the shared `RECALL_EVENTS_SQL`
        // constant) so this assertion can never drift from the query that
        // `recall_events` actually runs.
        let details = store
            .conn
            .prepare(&format!("EXPLAIN QUERY PLAN {RECALL_EVENTS_SQL}"))
            .expect("query plan prepares")
            .query_map(params![fts_query, 5_i64], |row| row.get::<_, String>(3))
            .expect("query plan runs")
            .collect::<Result<Vec<_>, _>>()
            .expect("query plan rows collect");

        assert!(
            details
                .iter()
                .any(|detail| detail.contains("VIRTUAL TABLE INDEX")),
            "recall query plan did not use the raw_events_fts virtual-table index: {details:?}"
        );

        cleanup_db_files(&path);
    }

    #[test]
    #[ignore = "performance evidence fixture; run explicitly on an idle host"]
    fn recall_50k_raw_events_median_latency_under_m2_target() {
        // Seed a 50k raw-event corpus in a single transaction (direct inserts so
        // setup is cheap). Content is grouped into 200-row topics so a recall
        // query matches a bounded ~200-row subset — a representative query, NOT a
        // worst case: the planned 256-candidate cap is not implemented, so
        // production queries are currently unbounded and broad matches are slower
        // (latency scales with match-set size).
        //
        // The plan's recall SLO is sub-100 ms (p95 < 100 ms in the capability
        // tables; p99 < 100 ms in the M2 exit row). This fixture records the full
        // p50/p95/p99 distribution as dated evidence and asserts only on the
        // MEDIAN as a regression floor: on a shared dev host the wall-clock p95/p99
        // tail is dominated by scheduler contention from co-resident processes and
        // varies with host load, not recall cost, whereas the median (~0.5 ms
        // here) is a contention-robust measure of the algorithm's cost.
        const CORPUS: usize = 50_000;
        const GROUP: usize = 200;
        const SAMPLES: usize = 1_000;
        let path = temp_db_path("recall-latency");
        let mut store = Store::open(&path).expect("store opens");

        {
            let tx = store.conn.transaction().expect("seed transaction");
            tx.execute(
                "INSERT INTO sessions (id, agent, started_at) VALUES ('session-1', 'claude', 1000)",
                [],
            )
            .expect("seed session insert");
            for index in 0..CORPUS {
                let ts = 1_000_i64 + i64::try_from(index).expect("index fits i64");
                let content = format!(
                    "entry {index} topic{} wal busy_timeout checkpoint contention backlog",
                    index / GROUP
                );
                let payload = serde_json::json!({ "text": content.as_str() }).to_string();
                tx.execute(
                    "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
                     VALUES ('session-1', ?1, 'tool_result', 'observation', ?2, '{}')",
                    params![ts, payload],
                )
                .expect("seed raw_event insert");
                let raw_event_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
                    params![raw_event_id, content.as_str()],
                )
                .expect("seed fts insert");
            }
            tx.commit().expect("seed commit");
        }

        // "topic5" matches exactly one 200-row group — a bounded, realistic recall.
        for _ in 0..10 {
            store
                .recall_events("topic5", 10)
                .expect("warmup recall succeeds");
        }

        let mut durations = Vec::with_capacity(SAMPLES);
        for _ in 0..SAMPLES {
            let started = Instant::now();
            let result = store.recall_events("topic5", 10).expect("recall succeeds");
            durations.push(started.elapsed());
            assert!(
                !result.hits.is_empty(),
                "recall returned no hits over the seeded corpus"
            );
        }

        durations.sort_unstable();
        let p50 = durations[SAMPLES / 2];
        let p95 = durations[SAMPLES * 95 / 100];
        let p99 = durations[SAMPLES * 99 / 100];
        eprintln!("recall_50k_p50={p50:?} p95={p95:?} p99={p99:?}");
        assert!(
            p50 < Duration::from_millis(100),
            "median recall latency {p50:?} exceeded the 100ms M2 target"
        );

        cleanup_db_files(&path);
    }

    fn seed_embed_job(store: &mut Store, scheduled_at: i64, ts: i64, text: &str) -> (i64, i64) {
        let tx = store.conn.transaction().expect("seed tx begins");
        tx.execute(
            "INSERT INTO sessions (id, agent, started_at, event_count, status)
             VALUES ('seed', 'claude', ?1, 1, 'open')
             ON CONFLICT(id) DO UPDATE SET event_count = sessions.event_count + 1",
            params![ts],
        )
        .expect("seed session");
        tx.execute(
            "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
             VALUES ('seed', ?1, 'tool_result', 'observation', ?2, '{}')",
            params![ts, format!("{{\"text\":\"{text}\"}}")],
        )
        .expect("seed raw_event");
        let raw_event_id = tx.last_insert_rowid();
        tx.execute(
            "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
            params![raw_event_id, text],
        )
        .expect("seed fts");
        tx.execute(
            "INSERT INTO jobs (kind, priority, state, payload, scheduled_at)
             VALUES ('embed', 100, 'pending', ?1, ?2)",
            params![format!("{{\"raw_event_id\":{raw_event_id}}}"), scheduled_at],
        )
        .expect("seed job");
        let job_id = tx.last_insert_rowid();
        tx.commit().expect("seed commits");
        (job_id, raw_event_id)
    }

    #[test]
    fn lease_then_complete_writes_embedding_and_provider_usage() {
        use crate::adapters::{NullAdapter, ProviderAdapter, prompt_token_estimate};
        let path = temp_db_path("embed-complete");
        let mut store = Store::open(&path).expect("store opens");
        let (job_id, raw_event_id) = seed_embed_job(&mut store, 100, 1000, "embed me");

        let leased = store
            .lease_embed_jobs(10, 5000, 60_000)
            .expect("lease succeeds");
        assert_eq!(leased.len(), 1);
        assert_eq!(leased[0].job_id, job_id);
        assert_eq!(leased[0].raw_event_id, raw_event_id);
        assert_eq!(leased[0].attempts, 1);
        assert_eq!(leased[0].content, "embed me");

        let adapter = NullAdapter::new();
        let vectors = adapter
            .embed(std::slice::from_ref(&leased[0].content))
            .expect("embed succeeds");
        store
            .complete_embed_job(
                leased[0].job_id,
                leased[0].raw_event_id,
                EmbedProvider {
                    adapter_id: adapter.id(),
                    model_id: adapter.model_id(),
                },
                &vectors[0],
                prompt_token_estimate(&leased[0].content),
                5000,
            )
            .expect("complete succeeds");

        let (owner_type, owner_id, dim, vector_len): (String, String, i64, i64) = store
            .conn
            .query_row(
                "SELECT owner_type, owner_id, dim, length(vector) FROM embeddings
                 WHERE owner_id = ?1",
                params![raw_event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("embedding row exists");
        assert_eq!(owner_type, "raw_event");
        assert_eq!(owner_id, raw_event_id.to_string());
        assert_eq!(dim, 32);
        assert_eq!(vector_len, 128);

        let (adapter_name, op, est_cost, usage_job): (String, String, f64, i64) = store
            .conn
            .query_row(
                "SELECT adapter, op, est_cost, job_id FROM provider_usage WHERE job_id = ?1",
                params![job_id],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .expect("provider usage row exists");
        assert_eq!(adapter_name, "null");
        assert_eq!(op, "embed");
        assert_eq!(est_cost, 0.0);
        assert_eq!(usage_job, job_id);

        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .expect("job row exists");
        assert_eq!(state, "done");

        cleanup_db_files(&path);
    }

    #[test]
    fn embed_lease_is_exactly_once_under_concurrent_workers() {
        use crate::adapters::{NullAdapter, ProviderAdapter, prompt_token_estimate};
        use std::collections::HashSet;
        use std::sync::Arc;

        let path = temp_db_path("lease-exactly-once");
        let total: i64 = 200;
        {
            let mut store = Store::open(&path).expect("store opens");
            let tx = store.conn.transaction().expect("seed tx");
            tx.execute(
                "INSERT INTO sessions (id, agent, started_at, event_count, status)
                 VALUES ('seed', 'claude', 1000, ?1, 'open')",
                params![total],
            )
            .expect("seed session");
            for i in 0..total {
                tx.execute(
                    "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
                     VALUES ('seed', ?1, 'tool_result', 'observation', ?2, '{}')",
                    params![1000 + i, format!("{{\"text\":\"job {i}\"}}")],
                )
                .expect("seed raw_event");
                let raw_event_id = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
                    params![raw_event_id, format!("job {i} content")],
                )
                .expect("seed fts");
                tx.execute(
                    "INSERT INTO jobs (kind, priority, state, payload, scheduled_at)
                     VALUES ('embed', 100, 'pending', ?1, 100)",
                    params![format!("{{\"raw_event_id\":{raw_event_id}}}")],
                )
                .expect("seed job");
            }
            tx.commit().expect("seed commits");
        }

        let path = Arc::new(path);
        let mut handles = Vec::new();
        for _ in 0..4 {
            let path = Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                let mut store = Store::open(path.as_path()).expect("worker store opens");
                let adapter = NullAdapter::new();
                let mut claimed: Vec<i64> = Vec::new();
                loop {
                    let leased = store
                        .lease_embed_jobs(8, 10_000, 60_000)
                        .expect("lease succeeds");
                    if leased.is_empty() {
                        break;
                    }
                    for job in leased {
                        claimed.push(job.job_id);
                        let vectors = adapter
                            .embed(std::slice::from_ref(&job.content))
                            .expect("embed succeeds");
                        store
                            .complete_embed_job(
                                job.job_id,
                                job.raw_event_id,
                                EmbedProvider {
                                    adapter_id: adapter.id(),
                                    model_id: adapter.model_id(),
                                },
                                &vectors[0],
                                prompt_token_estimate(&job.content),
                                10_000,
                            )
                            .expect("complete succeeds");
                    }
                }
                claimed
            }));
        }

        let mut all_claimed: Vec<i64> = Vec::new();
        for handle in handles {
            all_claimed.extend(handle.join().expect("worker thread joins"));
        }

        assert_eq!(
            all_claimed.len() as i64,
            total,
            "every job claimed exactly once"
        );
        let unique: HashSet<i64> = all_claimed.iter().copied().collect();
        assert_eq!(unique.len() as i64, total, "no job double-claimed");

        let store = Store::open(path.as_path()).expect("verify store opens");
        let done: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state = 'done'",
                [],
                |row| row.get(0),
            )
            .expect("done count");
        assert_eq!(done, total);

        cleanup_db_files(path.as_path());
    }

    #[test]
    fn failed_embed_job_defers_with_backoff_then_dead_letters() {
        let path = temp_db_path("embed-fail");
        let mut store = Store::open(&path).expect("store opens");
        let (job_id, _raw_event_id) = seed_embed_job(&mut store, 100, 1000, "doomed");

        assert_eq!(
            store
                .fail_job(job_id, 1, "boom", 10_000, 5, 500)
                .expect("fail"),
            JobOutcome::Deferred {
                scheduled_at: 10_500
            }
        );
        assert_eq!(
            store
                .fail_job(job_id, 2, "boom", 10_000, 5, 500)
                .expect("fail"),
            JobOutcome::Deferred {
                scheduled_at: 11_000
            }
        );
        assert_eq!(
            store
                .fail_job(job_id, 3, "boom", 10_000, 5, 500)
                .expect("fail"),
            JobOutcome::Deferred {
                scheduled_at: 12_000
            }
        );
        assert_eq!(
            store
                .fail_job(job_id, 4, "boom", 10_000, 5, 500)
                .expect("fail"),
            JobOutcome::Deferred {
                scheduled_at: 14_000
            }
        );
        assert_eq!(
            store
                .fail_job(job_id, 5, "boom", 10_000, 5, 500)
                .expect("fail"),
            JobOutcome::Dead
        );

        let state: String = store
            .conn
            .query_row(
                "SELECT state FROM jobs WHERE id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .expect("job row exists");
        assert_eq!(state, "dead");

        cleanup_db_files(&path);
    }

    #[test]
    fn expired_lease_is_reclaimed_after_visibility_timeout() {
        let path = temp_db_path("lease-expired");
        let mut store = Store::open(&path).expect("store opens");
        let (job_id, _raw_event_id) = seed_embed_job(&mut store, 100, 1000, "expiry candidate");

        let first = store.lease_embed_jobs(10, 1000, 1000).expect("lease");
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].job_id, job_id);
        assert_eq!(first[0].attempts, 1);

        let not_expired = store.lease_embed_jobs(10, 1500, 1000).expect("lease");
        assert!(not_expired.is_empty(), "lease still valid at now=1500");

        let reclaimed = store.lease_embed_jobs(10, 3000, 1000).expect("lease");
        assert_eq!(reclaimed.len(), 1);
        assert_eq!(reclaimed[0].job_id, job_id);
        assert_eq!(reclaimed[0].attempts, 2);

        cleanup_db_files(&path);
    }

    #[test]
    fn lease_respects_limit_and_priority_then_scheduled_at_order() {
        let path = temp_db_path("lease-order");
        let mut store = Store::open(&path).expect("store opens");
        let (job1, _) = seed_embed_job(&mut store, 100, 1000, "first");
        let (job2, _) = seed_embed_job(&mut store, 200, 1001, "second");
        let (job3, _) = seed_embed_job(&mut store, 300, 1002, "third");
        // job3 was enqueued last and scheduled latest, but a lower priority number
        // wins outright over the scheduled_at tiebreak.
        store
            .conn
            .execute("UPDATE jobs SET priority = 10 WHERE id = ?1", params![job3])
            .expect("raise job3 priority");

        let leased = store.lease_embed_jobs(2, 10_000, 60_000).expect("lease");
        // The selection respects priority then scheduled_at: job3 (priority 10) wins
        // over its late scheduled_at, and job1 beats job2 on the scheduled_at tiebreak,
        // so job2 is left behind. (UPDATE ... RETURNING yields the claimed rows in id
        // order, not the ORDER BY order, so compare the selected set.)
        let mut ids: Vec<i64> = leased.iter().map(|job| job.job_id).collect();
        ids.sort_unstable();
        let mut expected = vec![job1, job3];
        expected.sort_unstable();
        assert_eq!(ids, expected);
        assert!(
            !ids.contains(&job2),
            "lower-priority job2 must be left behind"
        );

        cleanup_db_files(&path);
    }

    // Concept-clustering test double: maps related words to a shared dimension so
    // it carries a real semantic signal (unlike the production hash `null` adapter),
    // letting fixtures prove semantic rerank beats lexical without a real provider.
    struct ConceptAdapter;

    impl ConceptAdapter {
        fn vector(text: &str) -> Vec<f32> {
            let mut v = vec![0.0f32; 3];
            for word in text
                .split(|c: char| !c.is_alphanumeric())
                .filter(|w| !w.is_empty())
            {
                let dim = match word.to_lowercase().as_str() {
                    "lock" | "mutex" | "contention" | "deadlock" => Some(0),
                    "database" | "schema" | "wal" | "sqlite" | "db" => Some(1),
                    "report" | "dashboard" | "ui" | "view" => Some(2),
                    _ => None,
                };
                if let Some(dim) = dim {
                    v[dim] += 1.0;
                }
            }
            v
        }
    }

    impl ProviderAdapter for ConceptAdapter {
        fn id(&self) -> &'static str {
            "null"
        }
        fn model_id(&self) -> &str {
            "concept-3"
        }
        fn reachable(&self) -> bool {
            true
        }
        fn embeds_semantically(&self) -> bool {
            true
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, crate::adapters::AdapterError> {
            Ok(texts.iter().map(|t| Self::vector(t)).collect())
        }
    }

    #[derive(Default)]
    struct CountingConceptAdapter {
        calls: std::sync::atomic::AtomicUsize,
    }

    impl CountingConceptAdapter {
        fn calls(&self) -> usize {
            self.calls.load(std::sync::atomic::Ordering::Relaxed)
        }
    }

    impl ProviderAdapter for CountingConceptAdapter {
        fn id(&self) -> &'static str {
            "null"
        }
        fn model_id(&self) -> &str {
            "concept-3"
        }
        fn reachable(&self) -> bool {
            true
        }
        fn embeds_semantically(&self) -> bool {
            true
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, crate::adapters::AdapterError> {
            self.calls
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            ConceptAdapter.embed(texts)
        }
    }

    fn capture_id(store: &mut Store, ts: i64, text: &str) -> i64 {
        store
            .capture_event(test_event_with_text("s", ts, text))
            .expect("capture succeeds")
            .raw_event_id
    }

    /// Deterministic metered LLM test-double: summarizes to a fixed string and reports
    /// a nonzero per-token price so the dream spend cap can be exercised offline.
    struct SummarizingAdapter;
    impl crate::adapters::ProviderAdapter for SummarizingAdapter {
        fn id(&self) -> &'static str {
            "openai_compat"
        }
        fn model_id(&self) -> &str {
            "stub-llm"
        }
        fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, crate::adapters::AdapterError> {
            Ok(texts.iter().map(|_| vec![0.0f32]).collect())
        }
        fn reachable(&self) -> bool {
            true
        }
        fn summarize(
            &self,
            texts: &[String],
        ) -> Result<Option<String>, crate::adapters::AdapterError> {
            Ok(Some(format!("summary:{}", texts.join("|"))))
        }
        fn usd_per_1k_prompt_tokens(&self) -> f64 {
            1.0
        }
    }

    fn seed_memory(
        store: &Store,
        id: &str,
        kind: &str,
        last_accessed: i64,
        decay_at: i64,
        state: &str,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO memories
                    (id, kind, content, lifecycle_state, relevance_score, last_accessed_at,
                     access_count, decay_at, created_at, source_trust, decay_score, decay_recomputed_at)
                 VALUES (?1, ?2, ?3, ?4, 0.5, ?5, 0, ?6, ?5, 0.5, 1.0, 0)",
                params![id, kind, format!("content {id}"), state, last_accessed, decay_at],
            )
            .expect("seed memory");
    }

    #[test]
    fn consolidate_dedup_clusters_duplicate_raw_events() {
        use crate::adapters::NullAdapter;
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-consolidate");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "wal busy_timeout fix");
        capture_id(&mut store, 1001, "wal busy_timeout fix"); // duplicate text
        capture_id(&mut store, 1002, "vacuum schedule");

        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");

        assert_eq!(
            outcome.consolidated, 2,
            "two distinct texts collapse to two memories"
        );
        assert_eq!(outcome.status, "completed");
        let mems: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mems, 2);
        let vers: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM memory_versions", [], |r| r.get(0))
            .unwrap();
        assert_eq!(vers, 2, "one immutable v1 per memory");
        let pending: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM raw_events WHERE consolidated_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(pending, 0, "all raw_events consumed");
        cleanup_db_files(&path);
    }

    #[test]
    fn consolidate_with_llm_records_tokens_and_provider_usage() {
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-llm");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "first event text");
        capture_id(&mut store, 1001, "second event text");

        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 1000.0,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &SummarizingAdapter,
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");

        assert_eq!(outcome.consolidated, 2);
        assert!(outcome.tokens_used > 0, "LLM summarize recorded tokens");
        assert_eq!(outcome.status, "completed");
        let content: String = store
            .conn
            .query_row(
                "SELECT content FROM memories ORDER BY created_at, id LIMIT 1",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            content.starts_with("summary:"),
            "memory body is the LLM stub summary, got {content}"
        );
        let usage: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM provider_usage WHERE op = 'complete'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(usage, 2, "one complete-usage row per cluster");
        cleanup_db_files(&path);
    }

    #[test]
    fn spend_cap_degrades_consolidation_to_lexical() {
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-budget");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "alpha alpha alpha");
        capture_id(&mut store, 1001, "beta beta beta");
        capture_id(&mut store, 1002, "gamma gamma gamma");

        let now = 2_000_000_000_000i64;
        // Budget allows roughly one cluster's summarize; the rest must degrade to lexical.
        let budget = 0.005;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: budget,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &SummarizingAdapter,
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");

        assert_eq!(
            outcome.consolidated, 3,
            "all clusters consolidated (some lexically)"
        );
        assert_eq!(outcome.status, "budget_capped");
        let spend: f64 = store
            .conn
            .query_row(
                "SELECT COALESCE(SUM(est_cost), 0) FROM provider_usage",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            spend <= budget + 1e-12,
            "provider spend stays within the cap, got {spend}"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn dream_wallclock_cap_stops_with_partial() {
        use crate::adapters::NullAdapter;
        use crate::dream::{DreamOptions, dream_once};
        use std::cell::Cell;
        let path = temp_db_path("dream-wallclock");
        let mut store = Store::open(&path).expect("store opens");
        for i in 0..5 {
            capture_id(&mut store, 1000 + i, &format!("event number {i}"));
        }
        // Each clock read advances 10s; max_seconds=5 -> the deadline trips immediately.
        let t = Cell::new(1_000_000i64);
        let clock = || {
            let v = t.get();
            t.set(v + 10_000);
            v
        };
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 5,
        };
        let outcome = dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &clock,
        )
        .expect("dream runs");

        assert_eq!(outcome.status, "partial");
        let pending: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM raw_events WHERE consolidated_at IS NULL",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(pending > 0, "wall-clock cap leaves work for the next run");
        cleanup_db_files(&path);
    }

    #[test]
    fn decay_transitions_follow_canonical_order_over_due_rows() {
        let path = temp_db_path("dream-decay");
        let store = Store::open(&path).expect("store opens");
        let hl = 14 * 86_400_000i64; // observation half-life
        let now = 100 * 86_400_000i64;
        seed_memory(
            &store,
            "m_active",
            "observation",
            now - hl / 2,
            now - 1,
            "active",
        );
        seed_memory(
            &store,
            "m_decay",
            "observation",
            now - hl * 3 / 2,
            now - 1,
            "active",
        );
        seed_memory(
            &store,
            "m_dorm",
            "observation",
            now - hl * 3,
            now - 1,
            "active",
        );
        seed_memory(
            &store,
            "m_arch",
            "observation",
            now - hl * 10,
            now - 1,
            "active",
        );

        let mut store = store;
        let batch = store.decay_due(500, now).expect("decay runs");
        assert_eq!(batch.touched, 4);
        let state = |id: &str| -> String {
            store
                .conn
                .query_row(
                    "SELECT lifecycle_state FROM memories WHERE id = ?1",
                    params![id],
                    |r| r.get(0),
                )
                .unwrap()
        };
        assert_eq!(state("m_active"), "active");
        assert_eq!(state("m_decay"), "decaying");
        assert_eq!(state("m_dorm"), "dormant");
        assert_eq!(state("m_arch"), "archived");
        cleanup_db_files(&path);
    }

    #[test]
    fn decay_query_plan_uses_index_no_scan() {
        let path = temp_db_path("dream-explain");
        let store = Store::open(&path).expect("store opens");
        // Seed a row so the planner reflects the populated plan, not the empty-table fast path.
        seed_memory(&store, "dummy", "observation", 0, 0, "active");
        let plan: Vec<String> = {
            let mut stmt = store
                .conn
                .prepare(
                    "EXPLAIN QUERY PLAN SELECT id, kind, COALESCE(last_accessed_at, created_at), \
                     access_count, source_trust FROM memories \
                     WHERE lifecycle_state IN ('active','associated','decaying','dormant') \
                     AND decay_at IS NOT NULL AND decay_at <= ?1 ORDER BY decay_at ASC LIMIT ?2",
                )
                .unwrap();
            stmt.query_map(params![0i64, 10i64], |r| r.get::<_, String>(3))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        let joined = plan.join(" | ");
        assert!(
            joined.contains("USING INDEX"),
            "decay must use an index, got: {joined}"
        );
        assert!(
            !joined.contains("SCAN"),
            "decay must not full-scan, got: {joined}"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn dream_run_records_accounting() {
        use crate::adapters::NullAdapter;
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-accounting");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "one");
        capture_id(&mut store, 1001, "two");
        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");

        let (trigger, jobs_run, touched, status, finished): (
            String,
            i64,
            i64,
            String,
            Option<i64>,
        ) = store
            .conn
            .query_row(
                "SELECT trigger, jobs_run, memories_touched, status, finished_at \
                     FROM dream_runs WHERE id = ?1",
                params![outcome.run_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?, r.get(4)?)),
            )
            .unwrap();
        assert_eq!(trigger, "manual");
        assert_eq!(
            touched, 2,
            "two distinct raw_events consolidate into two memories"
        );
        // jobs_run counts batch operations, not rows: one consolidate batch + one
        // associate batch (both same-session memories link), no decay batch (fresh
        // memories are not yet due) -> distinct from memories_touched.
        assert_eq!(
            jobs_run, 2,
            "jobs_run is the batch count (consolidate + associate), not the row count"
        );
        assert_eq!(status, "completed");
        assert!(finished.is_some(), "finished_at is stamped");
        cleanup_db_files(&path);
    }

    #[test]
    fn consolidation_versions_are_immutable_through_decay() {
        use crate::adapters::NullAdapter;
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-immutable");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "durable knowledge one");
        capture_id(&mut store, 1001, "durable knowledge two");
        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 60,
        };
        dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream");

        let before: Vec<(String, String)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT content, reason FROM memory_versions ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };

        // Force the memories due, then decay them.
        store
            .conn
            .execute("UPDATE memories SET decay_at = 1", [])
            .unwrap();
        store.decay_due(500, now).expect("decay");

        let after: Vec<(String, String)> = {
            let mut stmt = store
                .conn
                .prepare("SELECT content, reason FROM memory_versions ORDER BY id")
                .unwrap();
            stmt.query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
                .unwrap()
                .collect::<Result<Vec<_>, _>>()
                .unwrap()
        };
        assert_eq!(before.len(), 2);
        assert_eq!(
            before, after,
            "decay never rewrites or adds memory_versions"
        );
        cleanup_db_files(&path);
    }

    fn temp_jsonl(name: &str, lines: &[&str]) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        let path = std::env::temp_dir().join(format!(
            "memoryd-import-{name}-{}-{nanos}.jsonl",
            std::process::id()
        ));
        fs::write(&path, lines.join("\n")).expect("write jsonl fixture");
        path
    }

    fn count_imported(store: &Store) -> i64 {
        store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM raw_events WHERE kind = 'import'",
                [],
                |row| row.get(0),
            )
            .expect("import row count")
    }

    #[test]
    fn import_jsonl_stages_units_with_provenance_and_embed_jobs() {
        let db = temp_db_path("import-stage");
        let src = temp_jsonl(
            "stage",
            &[
                "{\"text\":\"wal busy_timeout fix\",\"session_id\":\"imp-1\",\"ts_ms\":1000}",
                "{\"text\":\"vacuum schedule\",\"session_id\":\"imp-1\",\"ts_ms\":1001}",
                "{\"text\":\"index plan review\",\"ts_ms\":1002}",
            ],
        );
        let mut store = Store::open(&db).expect("store opens");

        let summary = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("import succeeds");
        assert_eq!(summary.total, 3);
        assert_eq!(summary.processed, 3);
        assert_eq!(summary.skipped, 0);
        assert_eq!(summary.state, "completed");

        assert_eq!(count_imported(&store), 3);
        let embed_jobs: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE kind = 'embed'",
                [],
                |row| row.get(0),
            )
            .expect("embed job count");
        assert_eq!(
            embed_jobs, 3,
            "imported rows flow through the normal embed queue"
        );

        let provenance: String = store
            .conn
            .query_row(
                "SELECT provenance FROM raw_events WHERE kind = 'import' ORDER BY id LIMIT 1",
                [],
                |row| row.get(0),
            )
            .expect("provenance");
        let prov: serde_json::Value = serde_json::from_str(&provenance).expect("provenance json");
        assert_eq!(prov["import_source"], "jsonl");
        assert_eq!(prov["import_batch"], summary.batch_id);
        assert!(
            prov["path"].as_str().expect("path str").ends_with(".jsonl"),
            "source path is preserved in provenance"
        );

        // Imported text is reachable through the same lexical recall path as capture.
        let recall = store.recall_events("wal", 5).expect("recall");
        assert!(!recall.hits.is_empty(), "imported content is recall-able");

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn reimport_jsonl_is_idempotent_no_duplicate_rows() {
        let db = temp_db_path("import-idem");
        let src = temp_jsonl(
            "idem",
            &[
                "{\"text\":\"alpha\",\"ts_ms\":1}",
                "{\"text\":\"beta\",\"ts_ms\":2}",
            ],
        );
        let mut store = Store::open(&db).expect("store opens");

        let first = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("first import");
        assert_eq!(first.processed, 2);
        assert_eq!(first.skipped, 0);

        let second = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("second import");
        assert_eq!(
            second.batch_id, first.batch_id,
            "re-import reuses the batch row"
        );
        assert_eq!(second.processed, 0, "re-import stages nothing new");
        assert_eq!(second.skipped, 2, "both units dedup on re-import");
        assert_eq!(second.state, "completed");

        assert_eq!(count_imported(&store), 2, "no duplicate rows");
        let batches: i64 = store
            .conn
            .query_row("SELECT COUNT(*) FROM import_batches", [], |row| row.get(0))
            .expect("batch count");
        assert_eq!(batches, 1);

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn import_jsonl_dedups_duplicate_lines_within_file() {
        let db = temp_db_path("import-dup");
        let src = temp_jsonl(
            "dup",
            &[
                "{\"text\":\"same content\",\"ts_ms\":1}",
                "{\"text\":\"same content\",\"ts_ms\":2}",
                "{\"text\":\"other\",\"ts_ms\":3}",
            ],
        );
        let mut store = Store::open(&db).expect("store opens");

        let summary = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("import succeeds");
        assert_eq!(summary.total, 3);
        assert_eq!(summary.processed, 2, "identical text stages once");
        assert_eq!(summary.skipped, 1);
        assert_eq!(count_imported(&store), 2);

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn interrupted_import_resumes_without_duplicates() {
        let db = temp_db_path("import-resume");
        let mut store = Store::open(&db).expect("store opens");
        let src = temp_jsonl(
            "resume",
            &[
                "{\"text\":\"unit one\",\"ts_ms\":1}",
                "{\"text\":\"unit two\",\"ts_ms\":2}",
            ],
        );

        let first = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("first import");
        assert_eq!(first.processed, 2);
        assert_eq!(first.state, "completed");

        // Source grew (e.g. a live transcript appended-to); re-import the superset.
        fs::write(
            &src,
            [
                "{\"text\":\"unit one\",\"ts_ms\":1}",
                "{\"text\":\"unit two\",\"ts_ms\":2}",
                "{\"text\":\"unit three\",\"ts_ms\":3}",
                "{\"text\":\"unit four\",\"ts_ms\":4}",
            ]
            .join("\n"),
        )
        .expect("grow fixture");

        let second = store
            .import_jsonl("jsonl", &src, usize::MAX)
            .expect("resumed import");
        assert_eq!(second.batch_id, first.batch_id);
        assert_eq!(second.total, 4);
        assert_eq!(second.processed, 2, "two new units staged this run");
        assert_eq!(second.skipped, 2, "two already-seen units skip");
        assert_eq!(second.state, "completed");
        assert_eq!(count_imported(&store), 4, "no duplicates after resume");

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn import_pauses_when_embed_queue_is_full_then_resumes() {
        let db = temp_db_path("import-governed");
        let src = temp_jsonl(
            "governed",
            &[
                "{\"text\":\"one\",\"ts_ms\":1}",
                "{\"text\":\"two\",\"ts_ms\":2}",
                "{\"text\":\"three\",\"ts_ms\":3}",
            ],
        );
        let mut store = Store::open(&db).expect("store opens");

        // Queue cap of 2: the third unit cannot enqueue, so the batch pauses.
        let first = store.import_jsonl("jsonl", &src, 2).expect("capped import");
        assert_eq!(first.state, "paused");
        assert_eq!(
            first.processed, 2,
            "governor bounds staging to the queue cap"
        );
        assert_eq!(count_imported(&store), 2);
        let active: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM jobs WHERE state IN ('pending', 'deferred', 'running')",
                [],
                |row| row.get(0),
            )
            .expect("active job count");
        assert_eq!(active, 2, "embed throughput is bounded during backfill");

        // Worker drains the queue; resuming completes the batch.
        store
            .conn
            .execute("UPDATE jobs SET state = 'done' WHERE state = 'pending'", [])
            .expect("drain queue");
        let second = store
            .import_jsonl("jsonl", &src, 2)
            .expect("resumed import");
        assert_eq!(second.state, "completed");
        assert_eq!(second.processed, 1, "one remaining unit staged this run");
        assert_eq!(
            second.skipped, 2,
            "the two already-staged units dedup on resume"
        );
        assert_eq!(count_imported(&store), 3, "no duplicates after resume");

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn semantic_recall_outranks_lexical_on_labeled_fixture() {
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("recall-semantic-uplift");
        let mut store = Store::open(&path).expect("store opens");
        let adapter = ConceptAdapter;

        // All three share the token "lock" (so all enter the FTS shortlist); the
        // query "lock deadlock" matches only "lock" in each (none contain "deadlock"),
        // so the lexical scores tie and fall back to ts DESC.
        let a = capture_id(&mut store, 1000, "lock mutex contention"); // pure locking
        let _b = capture_id(&mut store, 1001, "lock database schema");
        let c = capture_id(&mut store, 1002, "lock report dashboard"); // newest
        for (id, text) in [
            (a, "lock mutex contention"),
            (_b, "lock database schema"),
            (c, "lock report dashboard"),
        ] {
            let v = adapter.embed(&[text.to_string()]).expect("embed");
            Store::store_embedding(
                &store.conn,
                "raw_event",
                &id.to_string(),
                adapter.model_id(),
                &v[0],
                1000,
            )
            .expect("store embedding");
        }

        let lexical = store
            .recall_events("lock deadlock", 1)
            .expect("lexical recall");
        assert_eq!(lexical.mode, "lexical");
        assert_ne!(
            lexical.hits[0].raw_event_id, a,
            "lexical alone should not surface the true match first"
        );

        let semantic = store
            .recall_semantic("lock deadlock", 1, &adapter, &BruteForce, 2000)
            .expect("semantic recall");
        assert_eq!(semantic.mode, "semantic");
        assert_eq!(
            semantic.hits[0].raw_event_id, a,
            "semantic rerank surfaces the concept-relevant match"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn query_embedding_is_cached_with_no_second_provider_call() {
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("recall-semantic-cache");
        let mut store = Store::open(&path).expect("store opens");
        let id = capture_id(&mut store, 1000, "lock mutex");
        let seed = ConceptAdapter
            .embed(&["lock mutex".to_string()])
            .expect("embed");
        Store::store_embedding(
            &store.conn,
            "raw_event",
            &id.to_string(),
            "concept-3",
            &seed[0],
            1000,
        )
        .expect("store embedding");

        let adapter = CountingConceptAdapter::default();
        assert_eq!(adapter.calls(), 0);
        store
            .recall_semantic("lock", 5, &adapter, &BruteForce, 2000)
            .expect("first recall");
        assert_eq!(
            adapter.calls(),
            1,
            "cache miss computes the query embedding once"
        );
        store
            .recall_semantic("lock", 5, &adapter, &BruteForce, 2001)
            .expect("second recall");
        assert_eq!(adapter.calls(), 1, "cache hit makes no provider call");

        let query_usage: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM provider_usage WHERE job_id IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("usage count");
        assert_eq!(query_usage, 1, "exactly one query-embed ledger row");

        cleanup_db_files(&path);
    }

    #[test]
    fn null_provider_degrades_semantic_to_lexical_same_shape() {
        use crate::adapters::NullAdapter;
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("recall-semantic-degrade");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "lock mutex");
        capture_id(&mut store, 1001, "lock schema");

        let lexical = store.recall_events("lock", 5).expect("lexical recall");
        let semantic = store
            .recall_semantic("lock", 5, &NullAdapter::new(), &BruteForce, 2000)
            .expect("semantic recall");

        assert_eq!(semantic.mode, "lexical");
        assert!(semantic.degraded);
        assert_eq!(semantic.compared, 0);
        let lex_ids: Vec<i64> = lexical.hits.iter().map(|h| h.raw_event_id).collect();
        let sem_ids: Vec<i64> = semantic.hits.iter().map(|h| h.raw_event_id).collect();
        assert_eq!(lex_ids, sem_ids, "degraded semantic matches lexical order");

        let query_rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE owner_type = 'query'",
                [],
                |row| row.get(0),
            )
            .expect("query embedding count");
        assert_eq!(query_rows, 0, "degrade path makes no provider call");

        cleanup_db_files(&path);
    }

    #[test]
    fn semantic_recall_with_no_fts_matches_returns_empty() {
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("recall-semantic-empty");
        let mut store = Store::open(&path).expect("store opens");
        capture_id(&mut store, 1000, "lock mutex");

        let result = store
            .recall_semantic("zzzznomatch", 5, &ConceptAdapter, &BruteForce, 2000)
            .expect("semantic recall");

        // Empty shortlist short-circuits before the query embedding: an attempted but
        // empty semantic result, not a degrade.
        assert!(result.hits.is_empty());
        assert_eq!(result.compared, 0);
        assert_eq!(result.mode, "semantic");
        assert!(!result.degraded);

        let query_rows: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE owner_type = 'query'",
                [],
                |row| row.get(0),
            )
            .expect("query embedding count");
        assert_eq!(query_rows, 0, "empty shortlist makes no provider call");

        cleanup_db_files(&path);
    }

    #[test]
    fn semantic_recall_compares_at_most_candidate_cap() {
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("recall-semantic-cap");
        let mut store = Store::open(&path).expect("store opens");
        // Bulk-seed 300 raw events that all match "lock", each with an embedding.
        {
            let tx = store.conn.transaction().expect("seed tx");
            tx.execute(
                "INSERT INTO sessions (id, agent, started_at, event_count, status)
                 VALUES ('seed', 'claude', 1000, 300, 'open')",
                [],
            )
            .expect("seed session");
            let mut bytes = Vec::new();
            for f in [1.0f32, 0.0, 0.0] {
                bytes.extend_from_slice(&f.to_le_bytes());
            }
            for i in 0..300i64 {
                tx.execute(
                    "INSERT INTO raw_events (session_id, ts, source, kind, payload, provenance)
                     VALUES ('seed', ?1, 'tool_result', 'observation', ?2, '{}')",
                    params![1000 + i, format!("{{\"text\":\"lock item{i}\"}}")],
                )
                .expect("seed raw_event");
                let rid = tx.last_insert_rowid();
                tx.execute(
                    "INSERT INTO raw_events_fts (raw_event_id, content) VALUES (?1, ?2)",
                    params![rid, format!("lock item{i}")],
                )
                .expect("seed fts");
                tx.execute(
                    "INSERT INTO embeddings (id, owner_type, owner_id, model_id, dim, vector, created_at)
                     VALUES (lower(hex(randomblob(16))), 'raw_event', ?1, 'concept-3', 3, ?2, 1000)",
                    params![rid.to_string(), bytes],
                )
                .expect("seed embedding");
            }
            tx.commit().expect("seed commits");
        }

        let result = store
            .recall_semantic("lock", 5, &ConceptAdapter, &BruteForce, 2000)
            .expect("semantic recall");
        assert_eq!(result.mode, "semantic");
        assert_eq!(
            result.compared, 256,
            "candidate comparison is capped at RECALL_CANDIDATE_CAP"
        );
        assert_eq!(result.hits.len(), 5);

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

    use crate::adapters::NullAdapter;

    // Mirror of the dream-plane association constants for assertions (private to dream).
    const CO_OCCUR_BASE_T: f64 = 0.30;
    const CO_OCCUR_REINFORCE_T: f64 = 0.10;
    const FANOUT_CAP_T: i64 = 32;

    // ---- M7: association graph + one-hop recall ----

    fn capture_in(store: &mut Store, session: &str, ts: i64, text: &str) -> i64 {
        store
            .capture_event(test_event_with_text(session, ts, text))
            .expect("capture succeeds")
            .raw_event_id
    }

    /// Seed a durable, recall-searchable memory directly (memories + memories_fts).
    fn seed_mem(
        store: &Store,
        id: &str,
        kind: &str,
        content: &str,
        session: Option<&str>,
        state: &str,
        now: i64,
    ) {
        store
            .conn
            .execute(
                "INSERT INTO memories
                    (id, kind, content, lifecycle_state, relevance_score, last_accessed_at,
                     access_count, decay_at, created_at, source_trust, decay_score,
                     decay_recomputed_at, centrality, source_session)
                 VALUES (?1, ?2, ?3, ?4, 0.5, ?5, 0, NULL, ?5, 0.5, 1.0, 0, 0.0, ?6)",
                params![id, kind, content, state, now, session],
            )
            .expect("seed memory");
        store
            .conn
            .execute(
                "INSERT INTO memories_fts (memory_id, content) VALUES (?1, ?2)",
                params![id, content],
            )
            .expect("seed fts");
    }

    fn seed_link(store: &Store, src: &str, dst: &str, link_type: &str, weight: f64, now: i64) {
        store
            .conn
            .execute(
                "INSERT INTO memory_links
                    (id, src_memory_id, dst_memory_id, link_type, weight, last_reinforced_at)
                 VALUES (lower(hex(randomblob(16))), ?1, ?2, ?3, ?4, ?5)",
                params![src, dst, link_type, weight, now],
            )
            .expect("seed link");
    }

    fn link_count(store: &Store) -> i64 {
        store
            .conn
            .query_row("SELECT COUNT(*) FROM memory_links", [], |r| r.get(0))
            .unwrap()
    }

    #[test]
    fn associate_co_occurrence_links_same_session() {
        let path = temp_db_path("assoc-cooccur");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "wal busy timeout",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "vacuum schedule",
            Some("s1"),
            "active",
            now,
        );

        let batch = store
            .associate_pending(&NullAdapter::new(), now, now)
            .expect("associate runs");

        assert_eq!(batch.links_created, 1, "one logical co-occurrence pair");
        // Symmetric storage => two directed rows.
        assert_eq!(link_count(&store), 2);
        let (lt, w): (String, f64) = store
            .conn
            .query_row(
                "SELECT link_type, weight FROM memory_links WHERE src_memory_id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(lt, "co_occurrence");
        assert!(
            (w - CO_OCCUR_BASE_T).abs() < 1e-9,
            "fresh weight = base, got {w}"
        );
        // Both nodes promoted active -> associated with non-zero centrality.
        for id in ["a", "b"] {
            let (state, cen): (String, f64) = store
                .conn
                .query_row(
                    "SELECT lifecycle_state, centrality FROM memories WHERE id=?1",
                    params![id],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .unwrap();
            assert_eq!(state, "associated", "node {id} associated");
            assert!(cen > 0.0, "node {id} centrality > 0");
        }
        cleanup_db_files(&path);
    }

    #[test]
    fn associate_reinforces_co_occurrence_on_rerun() {
        let path = temp_db_path("assoc-reinforce");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "alpha text",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "beta text",
            Some("s1"),
            "active",
            now,
        );

        store
            .associate_pending(&NullAdapter::new(), now, now)
            .unwrap();
        let later = now + 60_000;
        let batch = store
            .associate_pending(&NullAdapter::new(), later, later)
            .expect("re-associate");

        assert_eq!(
            batch.links_reinforced, 1,
            "existing pair reinforced, not recreated"
        );
        assert_eq!(link_count(&store), 2, "still one symmetric pair (UNIQUE)");
        let (w, reinforced_at): (f64, i64) = store
            .conn
            .query_row(
                "SELECT weight, last_reinforced_at FROM memory_links WHERE src_memory_id='a'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert!(
            (w - (CO_OCCUR_BASE_T + CO_OCCUR_REINFORCE_T)).abs() < 1e-9,
            "reinforced weight, got {w}"
        );
        assert_eq!(reinforced_at, later, "last_reinforced_at advanced");
        cleanup_db_files(&path);
    }

    #[test]
    fn associate_semantic_links_via_concept_adapter() {
        let path = temp_db_path("assoc-semantic");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // Same concept word ("database"/"sqlite" -> dim 1), DIFFERENT sessions so only
        // the embedding-similarity path can link them.
        seed_mem(
            &store,
            "a",
            "observation",
            "database schema design",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "sqlite wal checkpoint",
            Some("s2"),
            "active",
            now,
        );

        let batch = store
            .associate_pending(&ConceptAdapter, now, now)
            .expect("associate runs");

        assert_eq!(
            batch.links_created, 1,
            "one semantic pair from shared concept"
        );
        let lt: String = store
            .conn
            .query_row(
                "SELECT link_type FROM memory_links WHERE src_memory_id='a'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(lt, "semantic");
        // Memory embeddings were cached (reused by semantic recall rerank).
        let mem_vecs: i64 = store
            .conn
            .query_row(
                "SELECT COUNT(*) FROM embeddings WHERE owner_type='memory'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(mem_vecs, 2);
        cleanup_db_files(&path);
    }

    #[test]
    fn associate_fanout_cap_bounds_links() {
        let path = temp_db_path("assoc-fanout");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // 40 same-session memories: a complete co-occurrence graph would give each node
        // 39 links; the fan-out cap must hold each node at <= ASSOCIATE_FANOUT_CAP.
        for i in 0..40 {
            seed_mem(
                &store,
                &format!("m{i:02}"),
                "observation",
                &format!("distinct memory body number {i}"),
                Some("s1"),
                "active",
                now,
            );
        }
        store
            .associate_pending(&NullAdapter::new(), now, now)
            .unwrap();

        let max_degree: i64 = store
            .conn
            .query_row(
                "SELECT MAX(c) FROM (SELECT COUNT(*) c FROM memory_links GROUP BY src_memory_id)",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            max_degree <= FANOUT_CAP_T,
            "per-node fan-out {max_degree} exceeds cap {FANOUT_CAP_T}"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn associate_prunes_weak_links() {
        let path = temp_db_path("assoc-weak");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // Different sessions => associate creates no co-occurrence link; the pre-existing
        // sub-floor link must be pruned.
        seed_mem(
            &store,
            "a",
            "observation",
            "alpha",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "beta",
            Some("s2"),
            "active",
            now,
        );
        seed_link(&store, "a", "b", "co_occurrence", 0.05, now);
        seed_link(&store, "b", "a", "co_occurrence", 0.05, now);
        assert_eq!(link_count(&store), 2);

        let batch = store
            .associate_pending(&NullAdapter::new(), now, now)
            .unwrap();

        assert_eq!(link_count(&store), 0, "weak links pruned");
        assert!(batch.links_pruned >= 2, "pruned count reported");
        cleanup_db_files(&path);
    }

    #[test]
    fn recall_memories_one_hop_surfaces_missed_neighbor() {
        let path = temp_db_path("recall-onehop");
        let store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // memA matches the query; memB does not, but is linked to A.
        seed_mem(
            &store,
            "a",
            "observation",
            "wal busy timeout fix",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "vacuum schedule weekly",
            Some("s1"),
            "active",
            now,
        );
        seed_link(&store, "a", "b", "co_occurrence", 0.5, now);
        seed_link(&store, "b", "a", "co_occurrence", 0.5, now);
        let adapter = NullAdapter::new();

        let direct = store
            .recall_memories("wal", 5, 0, &adapter, now)
            .expect("hops=0");
        assert_eq!(direct.mode, "memory");
        let direct_ids: Vec<&str> = direct.hits.iter().map(|h| h.memory_id.as_str()).collect();
        assert_eq!(
            direct_ids,
            vec!["a"],
            "hops=0 returns only the lexical match"
        );

        let expanded = store
            .recall_memories("wal", 5, 1, &adapter, now)
            .expect("hops=1");
        assert_eq!(expanded.mode, "memory+graph");
        let ids: std::collections::HashSet<&str> =
            expanded.hits.iter().map(|h| h.memory_id.as_str()).collect();
        assert!(
            ids.contains("a") && ids.contains("b"),
            "one-hop surfaces the neighbor B"
        );
        let b_hit = expanded.hits.iter().find(|h| h.memory_id == "b").unwrap();
        assert!(b_hit.via_hop, "B is flagged as reached via graph hop");
        assert!((b_hit.link_strength - 0.5).abs() < 1e-9);
        cleanup_db_files(&path);
    }

    #[test]
    fn recall_memories_empty_when_no_memory_matches() {
        let path = temp_db_path("recall-empty");
        let store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "vacuum schedule",
            Some("s1"),
            "active",
            now,
        );
        let adapter = NullAdapter::new();
        let result = store
            .recall_memories("nonexistentterm", 5, 1, &adapter, now)
            .unwrap();
        assert!(
            result.hits.is_empty(),
            "no lexical match => empty (caller falls back)"
        );
        assert!(!result.degraded);
        cleanup_db_files(&path);
    }

    #[test]
    fn dream_once_runs_associate_phase() {
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("dream-associate");
        let mut store = Store::open(&path).expect("store opens");
        // Two distinct-text events in the same session -> two memories, then a co-occ link.
        capture_in(&mut store, "s1", 1000, "wal busy timeout fix");
        capture_in(&mut store, "s1", 1001, "vacuum schedule weekly");
        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");

        assert_eq!(outcome.consolidated, 2);
        assert_eq!(outcome.associated, 2, "both memories gain a link");
        assert_eq!(link_count(&store), 2, "one symmetric co-occurrence pair");
        cleanup_db_files(&path);
    }

    // ---- M8: profile extraction behind the approvals gate (H6) ----

    fn count(store: &Store, sql: &str) -> i64 {
        store.conn.query_row(sql, [], |r| r.get(0)).unwrap()
    }

    #[test]
    fn extract_proposes_pending_approval_not_a_fact() {
        let path = temp_db_path("m8-propose");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "prefers flyway for migrations",
            None,
            "active",
            now,
        );

        let mut spend = 0.0;
        let batch = store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .expect("extract runs");

        assert_eq!(batch.proposed, 1);
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM approvals WHERE state='pending'"
            ),
            1
        );
        // H6: no profile_fact is written by extraction — only a proposal.
        assert_eq!(count(&store, "SELECT COUNT(*) FROM profile_facts"), 0);
        let (tt, tr): (String, String) = store
            .conn
            .query_row(
                "SELECT target_type, target_ref FROM approvals LIMIT 1",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .unwrap();
        assert_eq!(tt, "profile_fact");
        assert_eq!(tr, "preference:prefers-flyway-for-migrations");
        cleanup_db_files(&path);
    }

    #[test]
    fn extract_is_idempotent_no_duplicate_pending() {
        let path = temp_db_path("m8-idem");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "uses rust edition 2024",
            None,
            "active",
            now,
        );
        let mut spend = 0.0;
        store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .unwrap();
        let second = store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .expect("extract runs again");
        assert_eq!(second.proposed, 0, "no new proposal");
        assert_eq!(second.skipped, 1, "candidate skipped (already proposed)");
        assert_eq!(count(&store, "SELECT COUNT(*) FROM approvals"), 1);
        cleanup_db_files(&path);
    }

    #[test]
    fn approve_accept_commits_fact_citing_the_approval() {
        let path = temp_db_path("m8-accept");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "prefers tabs over spaces",
            None,
            "active",
            now,
        );
        let mut spend = 0.0;
        store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .unwrap();
        let approval_id: String = store
            .conn
            .query_row("SELECT id FROM approvals WHERE state='pending'", [], |r| {
                r.get(0)
            })
            .unwrap();

        let decision = store
            .decide_approval(&approval_id, true, now + 100)
            .expect("decide");
        assert_eq!(decision.state, "approved");
        assert!(decision.committed_fact);
        assert!(!decision.already_decided);

        let (fk, fv, appr, fstate): (String, String, String, String) = store
            .conn
            .query_row(
                "SELECT fact_key, fact_value, approval_id, state FROM profile_facts",
                [],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?)),
            )
            .unwrap();
        assert_eq!(fk, "preference:prefers-tabs-over-spaces");
        assert_eq!(fv, "prefers tabs over spaces");
        assert_eq!(
            appr, approval_id,
            "fact cites the approval that authorized it (H6)"
        );
        assert_eq!(fstate, "active");
        let astate: String = store
            .conn
            .query_row(
                "SELECT state FROM approvals WHERE id=?1",
                params![approval_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(astate, "approved");
        cleanup_db_files(&path);
    }

    #[test]
    fn approve_reject_writes_no_fact() {
        let path = temp_db_path("m8-reject");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "dislikes yaml",
            None,
            "active",
            now,
        );
        let mut spend = 0.0;
        store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .unwrap();
        let id: String = store
            .conn
            .query_row("SELECT id FROM approvals", [], |r| r.get(0))
            .unwrap();

        let decision = store
            .decide_approval(&id, false, now + 100)
            .expect("decide");
        assert_eq!(decision.state, "rejected");
        assert!(!decision.committed_fact);
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM profile_facts"),
            0,
            "H6: reject writes no fact"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn approve_accept_supersedes_prior_active_fact() {
        let path = temp_db_path("m8-supersede");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // Two memories with the SAME normalized key but different consolidation paths is
        // hard to force; instead drive two approvals for the same fact_key directly.
        let key = "preference:editor-vim";
        for (n, val) in [(1, "vim"), (2, "neovim")] {
            let aid: String = store
                .conn
                .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))
                .unwrap();
            let change = serde_json::json!({
                "fact_key": key, "fact_value": val, "confidence": 0.7, "source_memory_id": null
            })
            .to_string();
            store
                .conn
                .execute(
                    "INSERT INTO approvals (id, target_type, target_ref, proposed_change, state, requested_at)
                     VALUES (?1, 'profile_fact', ?2, ?3, 'pending', ?4)",
                    params![aid, key, change, now + n],
                )
                .unwrap();
            store.decide_approval(&aid, true, now + 10 * n).unwrap();
        }
        // Exactly one active fact for the key (UNIQUE-active holds), value = latest.
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM profile_facts WHERE fact_key='preference:editor-vim' AND state='active'"
            ),
            1
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM profile_facts WHERE state='superseded'"
            ),
            1
        );
        let active_val: String = store
            .conn
            .query_row(
                "SELECT fact_value FROM profile_facts WHERE fact_key='preference:editor-vim' AND state='active'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(active_val, "neovim");
        cleanup_db_files(&path);
    }

    #[test]
    fn h6_profile_fact_requires_an_approval_id() {
        let path = temp_db_path("m8-h6");
        let store = Store::open(&path).expect("store opens");
        // The NOT NULL approval_id FK structurally forbids an un-approved profile fact.
        let res = store.conn.execute(
            "INSERT INTO profile_facts (id, fact_key, fact_value, confidence, approval_id, state, created_at)
             VALUES (lower(hex(randomblob(16))), 'k', 'v', 0.5, NULL, 'active', 0)",
            [],
        );
        assert!(
            res.is_err(),
            "profile_fact without approval_id must be rejected (H6)"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn extract_llm_stub_records_usage_and_respects_spend_cap() {
        let path = temp_db_path("m8-llm");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "prefers structured logging",
            None,
            "active",
            now,
        );
        // Generous budget: the metered test-double refines the fact value and records usage.
        let mut spend = 0.0;
        let batch = store
            .extract_profile_pending(&SummarizingAdapter, 100.0, &mut spend, 500, now)
            .expect("extract runs");
        assert_eq!(batch.proposed, 1);
        assert!(batch.tokens > 0, "LLM path consumed tokens");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM provider_usage WHERE op='complete'"
            ),
            1
        );
        let change: String = store
            .conn
            .query_row("SELECT proposed_change FROM approvals", [], |r| r.get(0))
            .unwrap();
        assert!(
            change.contains("summary:"),
            "fact value refined by the stub LLM"
        );
        assert!(spend > 0.0, "window spend advanced");
        cleanup_db_files(&path);
    }

    #[test]
    fn extract_spend_cap_degrades_to_verbatim_content() {
        let path = temp_db_path("m8-cap");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "p1",
            "preference",
            "a long enough preference body to cost tokens",
            None,
            "active",
            now,
        );
        // Zero budget: the metered adapter cannot be used; fall back to verbatim content.
        let mut spend = 0.0;
        let batch = store
            .extract_profile_pending(&SummarizingAdapter, 0.0, &mut spend, 500, now)
            .expect("extract runs");
        assert!(batch.budget_hit, "spend cap binds");
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM provider_usage"),
            0,
            "no spend under cap=0"
        );
        let change: String = store
            .conn
            .query_row("SELECT proposed_change FROM approvals", [], |r| r.get(0))
            .unwrap();
        assert!(!change.contains("summary:"), "degraded to verbatim content");
        cleanup_db_files(&path);
    }

    #[test]
    fn decide_approval_unknown_id_errors_and_noop_when_decided() {
        let path = temp_db_path("m8-decide-edge");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        assert!(matches!(
            store.decide_approval("does-not-exist", true, now),
            Err(StoreError::ApprovalNotFound(_))
        ));
        seed_mem(
            &store,
            "p1",
            "preference",
            "likes ci gates",
            None,
            "active",
            now,
        );
        let mut spend = 0.0;
        store
            .extract_profile_pending(&NullAdapter::new(), 0.0, &mut spend, 500, now)
            .unwrap();
        let id: String = store
            .conn
            .query_row("SELECT id FROM approvals", [], |r| r.get(0))
            .unwrap();
        store.decide_approval(&id, true, now + 1).unwrap();
        let again = store
            .decide_approval(&id, false, now + 2)
            .expect("idempotent");
        assert!(again.already_decided, "second decision is a no-op");
        assert_eq!(
            again.state, "approved",
            "state unchanged from first decision"
        );
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM profile_facts WHERE state='active'"
            ),
            1
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn decide_approval_redacts_fact_value_before_persist() {
        let path = temp_db_path("p1-fact-redact");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        let secret = "ghp_abcdefghijklmnopqrstuvwxyz123456";
        // Insert the approval directly to simulate an upstream redaction miss.
        let proposed_change = serde_json::json!({
            "fact_key": "ci.token",
            "fact_value": format!("uses {secret} for CI"),
            "confidence": 0.9,
        })
        .to_string();
        store
            .conn
            .execute(
                "INSERT INTO approvals
                    (id, target_type, target_ref, proposed_change, state, requested_at)
                 VALUES ('appr-1', 'profile_fact', 'ci.token', ?1, 'pending', ?2)",
                params![proposed_change, now],
            )
            .expect("approval inserted");

        let decision = store
            .decide_approval("appr-1", true, now + 1)
            .expect("approval accepted");
        assert!(decision.committed_fact);

        let fact_value: String = store
            .conn
            .query_row("SELECT fact_value FROM profile_facts", [], |r| r.get(0))
            .expect("fact persisted");
        assert!(
            fact_value.contains(REDACTED),
            "secret replaced: {fact_value}"
        );
        assert!(!fact_value.contains(secret), "secret must not persist");
        let audit_detail: Option<String> = store
            .conn
            .query_row(
                "SELECT detail FROM audit_log WHERE action = 'approve_profile_fact'",
                [],
                |r| r.get(0),
            )
            .expect("audit row exists");
        let detail = audit_detail.expect("redaction detail recorded");
        assert!(detail.contains("fact_value_redactions"));
        assert!(!detail.contains(secret));
        cleanup_db_files(&path);
    }

    #[test]
    fn redacts_sixteen_digit_pan() {
        let result = redact_inline_string_with_count("card 4111111111111111 ok");
        assert_eq!(result.value, format!("card {REDACTED} ok"));
        assert_eq!(result.redactions, 1);
    }

    #[test]
    fn does_not_redact_thirteen_digit_timestamp() {
        let result = redact_inline_string_with_count("ts 1717171717171 ok");
        assert_eq!(result.value, "ts 1717171717171 ok");
        assert_eq!(result.redactions, 0);
    }

    #[test]
    fn redacts_lowercase_akia_key() {
        let result = redact_inline_string_with_count("key akiaabcdefghij1234567890 set");
        assert_eq!(result.value, format!("key {REDACTED} set"));
    }

    #[test]
    fn redacts_uppercase_github_prefix() {
        let result =
            redact_inline_string_with_count("token GHP_ABCDEFGHIJKLMNOPQRSTUVWXYZ123456 set");
        assert_eq!(result.value, format!("token {REDACTED} set"));
    }

    #[test]
    fn dream_once_runs_extract_profile_phase() {
        use crate::dream::{DreamOptions, dream_once};
        let path = temp_db_path("m8-dream-extract");
        let mut store = Store::open(&path).expect("store opens");
        // A captured 'preference' raw_event consolidates to a preference memory, which
        // the extract phase then proposes as a pending approval.
        store
            .capture_event(NewRawEvent {
                session_id: "s1".to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "preference".to_string(),
                payload: serde_json::json!({"text": "prefers immediate transactions"}),
                provenance: serde_json::json!({}),
                ts_ms: 1000,
            })
            .expect("capture");
        let now = 2_000_000_000_000i64;
        let opts = DreamOptions {
            trigger: "manual",
            budget_usd: 0.0,
            max_seconds: 60,
        };
        let outcome = dream_once(
            &mut store,
            &NullAdapter::new(),
            &crate::config::Caps::small(),
            &opts,
            &|| now,
        )
        .expect("dream runs");
        assert_eq!(outcome.consolidated, 1);
        assert_eq!(outcome.proposed, 1, "extract phase proposed the preference");
        assert_eq!(
            count(
                &store,
                "SELECT COUNT(*) FROM approvals WHERE state='pending'"
            ),
            1
        );
        assert_eq!(
            count(&store, "SELECT COUNT(*) FROM profile_facts"),
            0,
            "H6: nothing committed without approval"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn decide_approval_rejects_malformed_proposal_without_committing() {
        let path = temp_db_path("m8-malformed");
        let mut store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        // A JSON-valid but structurally-empty proposal must not commit an empty-keyed fact.
        let aid: String = store
            .conn
            .query_row("SELECT lower(hex(randomblob(16)))", [], |r| r.get(0))
            .unwrap();
        store
            .conn
            .execute(
                "INSERT INTO approvals (id, target_type, target_ref, proposed_change, state, requested_at)
                 VALUES (?1, 'profile_fact', 'x', '{}', 'pending', ?2)",
                params![aid, now],
            )
            .unwrap();
        assert!(matches!(
            store.decide_approval(&aid, true, now + 1),
            Err(StoreError::MalformedApproval(_))
        ));
        assert_eq!(count(&store, "SELECT COUNT(*) FROM profile_facts"), 0);
        let st: String = store
            .conn
            .query_row(
                "SELECT state FROM approvals WHERE id=?1",
                params![aid],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            st, "pending",
            "malformed accept rolled back, approval untouched"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn recall_semantic_with_hnsw_matches_brute_force_oracle() {
        use crate::vectorindex::{BruteForce, Hnsw};
        let path = temp_db_path("recall-semantic-hnsw");
        let mut store = Store::open(&path).expect("store opens");
        let adapter = ConceptAdapter;
        let a = capture_id(&mut store, 1000, "lock mutex contention");
        let b = capture_id(&mut store, 1001, "lock database schema");
        let c = capture_id(&mut store, 1002, "lock report dashboard");
        for (id, text) in [
            (a, "lock mutex contention"),
            (b, "lock database schema"),
            (c, "lock report dashboard"),
        ] {
            let v = adapter.embed(&[text.to_string()]).expect("embed");
            Store::store_embedding(
                &store.conn,
                "raw_event",
                &id.to_string(),
                adapter.model_id(),
                &v[0],
                1000,
            )
            .expect("store embedding");
        }
        let oracle = store
            .recall_semantic("lock deadlock", 3, &adapter, &BruteForce, 2000)
            .expect("brute recall");
        let hnsw = store
            .recall_semantic("lock deadlock", 3, &adapter, &Hnsw::default(), 2000)
            .expect("hnsw recall");
        let oracle_ids: Vec<i64> = oracle.hits.iter().map(|h| h.raw_event_id).collect();
        let hnsw_ids: Vec<i64> = hnsw.hits.iter().map(|h| h.raw_event_id).collect();
        assert_eq!(
            hnsw_ids, oracle_ids,
            "HNSW recall matches the BruteForce oracle"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn migration_0007_rebuild_preserves_rows_and_admits_local() {
        // Drive the rebuild SQL directly against a v1-vocabulary table with data in
        // place, proving rows survive and the widened CHECK admits 'local' only.
        let path = temp_db_path("mig-0007");
        let conn = Connection::open(&path).expect("conn opens");
        conn.execute_batch(MIGRATION_0001).expect("v1 schema");
        conn.execute(
            "INSERT INTO provider_usage
                (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
             VALUES (1000, 'null', 'null-hash-32', 'embed', 3, 0, 0.0, NULL)",
            [],
        )
        .expect("seed old-vocab row");
        conn.execute_batch(MIGRATION_0007)
            .expect("rebuild succeeds");

        let (count, adapter): (i64, String) = conn
            .query_row(
                "SELECT COUNT(*), MIN(adapter) FROM provider_usage",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .expect("rows queryable");
        assert_eq!(count, 1, "existing ledger rows survive the rebuild");
        assert_eq!(adapter, "null");

        conn.execute(
            "INSERT INTO provider_usage
                (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
             VALUES (2000, 'local', 'bge-small-en-v1.5', 'embed', 3, 0, 0.0, NULL)",
            [],
        )
        .expect("'local' is admitted after rebuild");
        let bogus = conn.execute(
            "INSERT INTO provider_usage
                (ts, adapter, model_id, op, prompt_tokens, completion_tokens, est_cost, job_id)
             VALUES (3000, 'bogus', 'x', 'embed', 0, 0, 0.0, NULL)",
            [],
        );
        assert!(bogus.is_err(), "unknown adapters still rejected by CHECK");
        cleanup_db_files(&path);
    }

    #[test]
    fn local_adapter_embeds_384_dim_with_local_ledger_row() {
        use crate::adapters::{LocalAdapter, ProviderAdapter, prompt_token_estimate};
        let path = temp_db_path("local-embed");
        let mut store = Store::open(&path).expect("store opens");
        let (job_id, raw_event_id) = seed_embed_job(
            &mut store,
            100,
            1000,
            "I prefer dark mode in all my editors",
        );

        let leased = store.lease_embed_jobs(10, 5000, 60_000).expect("lease");
        assert_eq!(leased.len(), 1);
        let adapter = LocalAdapter;
        let vectors = adapter
            .embed(std::slice::from_ref(&leased[0].content))
            .expect("local embed succeeds");
        store
            .complete_embed_job(
                job_id,
                raw_event_id,
                EmbedProvider {
                    adapter_id: adapter.id(),
                    model_id: adapter.model_id(),
                },
                &vectors[0],
                prompt_token_estimate(&leased[0].content),
                5000,
            )
            .expect("complete succeeds");

        let (dim, vector_len, model_id): (i64, i64, String) = store
            .conn
            .query_row(
                "SELECT dim, length(vector), model_id FROM embeddings WHERE owner_id = ?1",
                params![raw_event_id.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .expect("embedding row exists");
        assert_eq!(dim, 384, "bge-small dimension");
        assert_eq!(vector_len, 384 * 4, "f32 little-endian blob");
        assert_eq!(model_id, "bge-small-en-v1.5");

        let ledger_adapter: String = store
            .conn
            .query_row(
                "SELECT adapter FROM provider_usage WHERE job_id = ?1",
                params![job_id],
                |row| row.get(0),
            )
            .expect("ledger row exists");
        assert_eq!(ledger_adapter, "local", "ledger names the real provider");
        cleanup_db_files(&path);
    }

    #[test]
    fn recall_semantic_with_local_adapter_ranks_semantic_neighbor_first() {
        use crate::adapters::{LocalAdapter, ProviderAdapter, prompt_token_estimate};
        use crate::vectorindex::BruteForce;
        let path = temp_db_path("local-recall");
        let mut store = Store::open(&path).expect("store opens");
        let adapter = LocalAdapter;

        let texts = [
            "the user prefers dark themes in every editor",
            "the quarterly finance report is due next week",
        ];
        for (i, text) in texts.iter().enumerate() {
            let (job_id, raw_event_id) =
                seed_embed_job(&mut store, 100 + i as i64, 1000 + i as i64, text);
            let leased = store.lease_embed_jobs(10, 5000, 60_000).expect("lease");
            let job = leased
                .iter()
                .find(|j| j.job_id == job_id)
                .expect("seeded job leased");
            let vectors = adapter
                .embed(std::slice::from_ref(&job.content))
                .expect("embed");
            store
                .complete_embed_job(
                    job_id,
                    raw_event_id,
                    EmbedProvider {
                        adapter_id: adapter.id(),
                        model_id: adapter.model_id(),
                    },
                    &vectors[0],
                    prompt_token_estimate(&job.content),
                    5000,
                )
                .expect("complete");
        }

        // The query FTS-matches the dark-theme row; the rerank must keep semantic
        // mode (real signal, not degraded) and surface it.
        let result = store
            .recall_semantic("dark theme preference", 5, &adapter, &BruteForce, 6000)
            .expect("semantic recall");
        assert_eq!(result.mode, "semantic", "real signal => semantic mode");
        assert!(!result.degraded);
        assert!(
            !result.hits.is_empty(),
            "semantic recall returns the dark-theme memory"
        );
        cleanup_db_files(&path);
    }

    // ---- MCP: one-hop neighborhood lookup (memory_neighbors) ----

    #[test]
    fn memory_neighbors_returns_strongest_links_first() {
        let path = temp_db_path("neighbors-order");
        let store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "wal fix",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "vacuum schedule",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "c",
            "rule",
            "use flyway",
            Some("s1"),
            "associated",
            now,
        );
        seed_link(&store, "a", "b", "co_occurrence", 0.30, now);
        seed_link(&store, "a", "c", "semantic", 0.80, now + 5);

        let hood = store
            .memory_neighbors("a", 10)
            .expect("lookup succeeds")
            .expect("memory exists");

        assert_eq!(hood.memory_id, "a");
        assert_eq!(hood.kind, "observation");
        assert_eq!(hood.content, "wal fix");
        assert_eq!(hood.neighbors.len(), 2);
        assert_eq!(hood.neighbors[0].memory_id, "c", "strongest link first");
        assert_eq!(hood.neighbors[0].link_type, "semantic");
        assert!((hood.neighbors[0].link_strength - 0.80).abs() < 1e-9);
        assert_eq!(hood.neighbors[0].last_reinforced_at, now + 5);
        assert_eq!(hood.neighbors[0].lifecycle_state, "associated");
        assert_eq!(hood.neighbors[1].memory_id, "b");
        assert_eq!(hood.neighbors[1].link_type, "co_occurrence");
        assert!((hood.neighbors[1].link_strength - 0.30).abs() < 1e-9);
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_neighbors_unknown_id_returns_none() {
        let path = temp_db_path("neighbors-unknown");
        let store = Store::open(&path).expect("store opens");

        assert_eq!(store.memory_neighbors("nope", 10).expect("lookup"), None);
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_neighbors_filters_non_recallable_lifecycles() {
        let path = temp_db_path("neighbors-lifecycle");
        let store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "wal fix",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "old note",
            Some("s1"),
            "archived",
            now,
        );
        seed_mem(
            &store,
            "c",
            "observation",
            "live note",
            Some("s1"),
            "decaying",
            now,
        );
        seed_link(&store, "a", "b", "co_occurrence", 0.90, now);
        seed_link(&store, "a", "c", "co_occurrence", 0.30, now);

        let hood = store
            .memory_neighbors("a", 10)
            .expect("lookup succeeds")
            .expect("memory exists");
        assert_eq!(hood.neighbors.len(), 1, "archived neighbor is filtered");
        assert_eq!(hood.neighbors[0].memory_id, "c");

        // A non-recallable source id behaves like an unknown id.
        assert_eq!(store.memory_neighbors("b", 10).expect("lookup"), None);
        cleanup_db_files(&path);
    }

    #[test]
    fn memory_neighbors_clamps_limit() {
        let path = temp_db_path("neighbors-clamp");
        let store = Store::open(&path).expect("store opens");
        let now = 1_000_000_000_000i64;
        seed_mem(
            &store,
            "a",
            "observation",
            "wal fix",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "b",
            "observation",
            "first",
            Some("s1"),
            "active",
            now,
        );
        seed_mem(
            &store,
            "c",
            "observation",
            "second",
            Some("s1"),
            "active",
            now,
        );
        seed_link(&store, "a", "b", "co_occurrence", 0.80, now);
        seed_link(&store, "a", "c", "co_occurrence", 0.40, now);

        // limit 0 clamps up to 1 (never an empty/invalid LIMIT).
        let hood = store
            .memory_neighbors("a", 0)
            .expect("lookup succeeds")
            .expect("memory exists");
        assert_eq!(hood.neighbors.len(), 1);
        assert_eq!(hood.neighbors[0].memory_id, "b", "strongest survives clamp");

        // An oversized limit clamps down to 50 (no error, all rows still fit).
        let hood = store
            .memory_neighbors("a", 5_000)
            .expect("lookup succeeds")
            .expect("memory exists");
        assert_eq!(hood.neighbors.len(), 2);
        cleanup_db_files(&path);
    }
}
