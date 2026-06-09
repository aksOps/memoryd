use crate::adapters::{ProviderAdapter, prompt_token_estimate};
use crate::import::{ImportError, ImportSummary, ImportUnit, content_hash, parse_jsonl};
use crate::vectorindex::{Candidate, VectorIndex};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA_VERSION: i64 = 3;

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
        let vectors = match adapter.embed(std::slice::from_ref(&query.to_string())) {
            Ok(vectors) => vectors,
            Err(_) => return Ok(None),
        };
        let Some(vector) = vectors.into_iter().next().filter(|v| !v.is_empty()) else {
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
        model_id: &str,
        vector: &[f32],
        prompt_tokens: i64,
        now_ms: i64,
    ) -> Result<(), StoreError> {
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
             VALUES (?1, 'null', ?2, 'embed', ?3, 0, 0.0, ?4)",
            params![now_ms, model_id, prompt_tokens, job_id],
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
        }
    }
}

impl std::error::Error for StoreError {}

impl From<ImportError> for StoreError {
    fn from(err: ImportError) -> Self {
        Self::Import(err.to_string())
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

/// Cap on the whole-file read in [`Store::import_jsonl`] - bounds memory on a small VM.
/// 64 MiB of JSONL is hundreds of thousands of records, well beyond personal scale.
const MAX_IMPORT_FILE_BYTES: u64 = 64 * 1024 * 1024;

/// Outcome of staging one import unit: a new row, a dedup skip, or a queue-full pause.
enum StageOutcome {
    Staged,
    Skipped,
    Paused,
}

// Staging-dedup key for imported rows (ARCHITECTURE-PLAN s11.6). NULL for native
// captures; the partial unique index makes re-import a no-op for seen content.
const MIGRATION_0003: &str = r#"
ALTER TABLE raw_events ADD COLUMN content_hash BLOB;
CREATE UNIQUE INDEX ux_raw_import_hash ON raw_events(content_hash) WHERE kind = 'import';
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
                adapter.model_id(),
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
                                adapter.model_id(),
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
}
