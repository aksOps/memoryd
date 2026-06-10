#![forbid(unsafe_code)]

use memoryd_core::config::{Config, DEFAULT_BIND};
use memoryd_core::store::{
    ApprovalDecision, ApprovalRow, CaptureAck, MemoryRecallResult, NewRawEvent, RecallResult,
    Store, StoreError,
};
use memoryd_core::writer::WriterHandle;
use std::collections::HashMap;
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

mod logging;
mod mcp;

const MAX_HTTP_LINE_BYTES: usize = 8 * 1024;
const MAX_HTTP_HEADERS: usize = 64;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;
/// How often the `serve` dream scheduler runs a consolidate+decay pass.
const DREAM_INTERVAL_SECS: u64 = 300;
/// Socket deadlines for each accepted connection. Without these a client that
/// connects and never sends (or dribbles) bytes would pin its handler thread
/// forever — the classic slowloris. 10s is generous for a local daemon.
const HTTP_READ_TIMEOUT: Duration = Duration::from_secs(10);
const HTTP_WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Upper bound on concurrently handled connections; excess connections get an
/// immediate 503 instead of an unbounded thread pile-up.
const MAX_CONCURRENT_CONNECTIONS: usize = 64;
/// Auth throttle policy: this many 401s within the window locks the peer IP out
/// for the lockout duration. A successful request clears the peer's state.
const AUTH_FAIL_LIMIT: u32 = 5;
const AUTH_FAIL_WINDOW_MS: i64 = 60_000;
const AUTH_LOCKOUT_MS: i64 = 60_000;
/// Memory bound for the throttle map; expired entries are dropped first, then
/// the entry with the oldest failure is evicted.
const AUTH_THROTTLE_MAX_ENTRIES: usize = 1024;

fn main() -> ExitCode {
    match run(env::args_os()) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            eprintln!("memoryd: {err}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: impl IntoIterator<Item = OsString>) -> Result<(), CliError> {
    let cli = Cli::parse(args)?;
    match cli.command.clone() {
        Command::Doctor => doctor(cli),
        Command::Stats => stats(cli),
        Command::Serve => serve(cli),
        Command::Remember(args) => remember(cli, args),
        Command::Recall(args) => recall(cli, args),
        Command::Import(args) => import(cli, args),
        Command::Dream(args) => dream(cli, args),
        Command::Approve(args) => approve(cli, args),
        Command::Mcp => mcp::serve_stdio(cli),
        Command::Help => {
            print_help();
            Ok(())
        }
    }
}

fn doctor(cli: Cli) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let store = Store::open(&cfg.db_path)?;
    let report = store.doctor_report()?;

    println!("memoryd doctor");
    println!("db_path: {}", report.db_path.display());
    println!("schema_version: {}", report.schema_version);
    println!("journal_mode: {}", report.journal_mode);
    println!(
        "foreign_keys: {}",
        if report.foreign_keys { "on" } else { "off" }
    );
    println!("integrity_check: {}", report.integrity_check);
    println!("missing_tables: {}", report.missing_tables.len());
    println!("bind: {}", cfg.bind);
    println!("provider: {}", cfg.providers.default_adapter);
    println!("paid_spend_cap_usd: {:.2}", cfg.caps.paid_spend_cap_usd);

    if report.is_ok() {
        Ok(())
    } else {
        Err(CliError::DoctorFailed)
    }
}

fn stats(cli: Cli) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let store = Store::open(&cfg.db_path)?;
    println!("memoryd stats");
    for stat in store.table_stats()? {
        println!("{}: {}", stat.table, stat.rows);
    }
    Ok(())
}

fn remember(cli: Cli, args: RememberArgs) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let mut store = Store::open(&cfg.db_path)?;
    let ack =
        store.capture_event_with_queue_limit(remember_event(args), cfg.caps.queue_depth_max)?;
    println!("{}", remember_response_json(&ack)?);
    Ok(())
}

fn recall(cli: Cli, args: RecallArgs) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let store = Store::open(&cfg.db_path)?;
    let index_kind = args
        .index_kind
        .clone()
        .unwrap_or_else(|| cfg.caps.vector_index_kind.clone());
    if !matches!(index_kind.as_str(), "brute-force" | "hnsw") {
        return Err(CliError::Config(
            memoryd_core::config::ConfigError::UnknownVectorIndex { kind: index_kind },
        ));
    }
    let adapter = memoryd_core::adapters::AdapterKind::from_provider_config(&cfg.providers);
    let result = recall_with_mode(&store, &args, &index_kind, &adapter)?;
    println!("{}", recall_response_json(&result)?);
    Ok(())
}

/// Backfill historic data through the same capture path. Only the generic JSONL
/// format ships in this slice; source-specific importers are deferred until it is
/// stable. The embed queue is bounded by the governor's `queue_depth_max`, so a
/// large import pauses and resumes rather than flooding the worker.
fn import(cli: Cli, args: ImportArgs) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    if args.path.is_empty() {
        return Err(CliError::MissingArgument("--path"));
    }
    if args.source != "jsonl" {
        // Don't echo the user-supplied value (it could contain a pasted secret).
        return Err(CliError::Store(StoreError::Import(
            "unsupported import source; only \"jsonl\" is supported in this build".to_string(),
        )));
    }

    let mut store = Store::open(&cfg.db_path)?;
    let summary = store.import_jsonl(
        &args.source,
        &PathBuf::from(&args.path),
        cfg.caps.queue_depth_max,
    )?;
    println!("{}", import_response_json(&summary)?);
    Ok(())
}

/// Run one dream pass now: consolidate pending raw_events into durable memories and
/// decay due memories, under the wall-clock + spend caps (overridable via flags).
/// The adapter comes from config (`local` default: in-process embeddings, lexical
/// consolidation, no spend; `null`: fully inert).
fn dream(cli: Cli, args: DreamArgs) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let mut store = Store::open(&cfg.db_path)?;
    let adapter = memoryd_core::adapters::AdapterKind::from_provider_config(&cfg.providers);
    let opts = memoryd_core::dream::DreamOptions {
        trigger: "manual",
        budget_usd: args.budget_usd.unwrap_or(cfg.caps.paid_spend_cap_usd),
        max_seconds: args.max_seconds.unwrap_or(cfg.caps.dream_wallclock_secs),
    };
    let outcome =
        memoryd_core::dream::dream_once(&mut store, &adapter, &cfg.caps, &opts, &|| unix_ms_now())?;
    println!("{}", dream_response_json(&outcome)?);
    Ok(())
}

/// Human-in-the-loop approvals gate (H6). `--list` (the default) shows pending
/// approvals; `--id <id> --accept|--reject` decides one. Accepting a `profile_fact`
/// commits it to `profile_facts`; rejecting writes no fact.
fn approve(cli: Cli, args: ApproveArgs) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;
    if args.accept && args.reject {
        return Err(CliError::UnexpectedArgument(
            "--reject (use only one of --accept/--reject)".to_string(),
        ));
    }
    let mut store = Store::open(&cfg.db_path)?;
    match &args.id {
        Some(id) => {
            if !args.accept && !args.reject {
                return Err(CliError::MissingArgument(
                    "--accept or --reject (required with --id)",
                ));
            }
            let decision = store.decide_approval(id, args.accept, unix_ms_now())?;
            println!("{}", approve_decision_json(id, &decision)?);
        }
        None => {
            if args.accept || args.reject {
                return Err(CliError::MissingArgument(
                    "--id (required with --accept/--reject)",
                ));
            }
            let pending = store.list_pending_approvals(100)?;
            println!("{}", approve_list_json(&pending)?);
        }
    }
    Ok(())
}

fn approve_list_json(pending: &[ApprovalRow]) -> Result<String, CliError> {
    let items: Vec<serde_json::Value> = pending
        .iter()
        .map(|a| {
            let change: serde_json::Value =
                serde_json::from_str(&a.proposed_change).unwrap_or(serde_json::Value::Null);
            serde_json::json!({
                "id": a.id,
                "target_type": a.target_type,
                "target_ref": a.target_ref,
                "proposed_change": change,
                "requested_at": a.requested_at,
            })
        })
        .collect();
    Ok(serde_json::to_string(
        &serde_json::json!({ "pending": items }),
    )?)
}

fn approve_decision_json(id: &str, decision: &ApprovalDecision) -> Result<String, CliError> {
    Ok(serde_json::to_string(&serde_json::json!({
        "id": id,
        "state": decision.state,
        "committed_fact": decision.committed_fact,
        "already_decided": decision.already_decided,
    }))?)
}

/// Run semantic recall when requested, else lexical. The `null` adapter
/// self-degrades to lexical (`embeds_semantically` is false), so `--semantic`
/// is safe by default; `local` and a configured `openai_compat` endpoint
/// activate real rerank with no caller change.
fn recall_with_mode(
    store: &Store,
    args: &RecallArgs,
    index_kind: &str,
    adapter: &memoryd_core::adapters::AdapterKind,
) -> Result<RecallOutput, StoreError> {
    // Prefer durable memory + graph recall; fall back to raw-event recall when the
    // memory corpus has no match (e.g. before any dream run) so M2 behavior is preserved.
    let memory =
        store.recall_memories(&args.query, args.limit, args.hops, adapter, unix_ms_now())?;
    if !memory.hits.is_empty() {
        return Ok(RecallOutput::Memory(memory));
    }
    let event = if args.semantic {
        let index = memoryd_core::vectorindex::from_kind(index_kind);
        store.recall_semantic(
            &args.query,
            args.limit,
            adapter,
            index.as_ref(),
            unix_ms_now(),
        )?
    } else {
        store.recall_events(&args.query, args.limit)?
    };
    Ok(RecallOutput::Event(event))
}

/// Recall returns either durable memories (with optional graph expansion) or raw
/// events (the M2 degrade path) depending on whether the memory corpus matched.
enum RecallOutput {
    Memory(MemoryRecallResult),
    Event(RecallResult),
}

fn serve(cli: Cli) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    // The single-writer actor (ARCHITECTURE-PLAN s7.1/U5): HTTP capture, auth
    // audit, and the embed worker route writes through this one thread.
    // Writer::spawn opens the store, so startup still fails fast and runs
    // migrations before any background thread starts.
    let (writer, writer_thread) = memoryd_core::writer::Writer::spawn(&cfg.db_path)?;

    let shutdown = Arc::new(AtomicBool::new(false));
    for signal in [signal_hook::consts::SIGTERM, signal_hook::consts::SIGINT] {
        signal_hook::flag::register(signal, Arc::clone(&shutdown))?;
    }

    let worker_caps = cfg.caps.clone();
    let worker_providers = cfg.providers.clone();
    let worker_shutdown = Arc::clone(&shutdown);
    let mut worker_access = writer.clone();
    // The embed worker leases/completes jobs through the writer actor; only
    // the embedding compute runs on this thread. It drains its in-flight tick
    // and exits on shutdown.
    let worker = std::thread::spawn(move || {
        let adapter = memoryd_core::adapters::AdapterKind::from_provider_config(&worker_providers);
        while !worker_shutdown.load(Ordering::Acquire) {
            let now = unix_ms_now();
            match memoryd_core::worker::tick_embed(&mut worker_access, &adapter, &worker_caps, now)
            {
                Ok(report) if report.leased == 0 => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Ok(_) => {}
                Err(err) => {
                    logging::log_warn!("worker tick failed: {err}");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    });

    let dream_db = cfg.db_path.clone();
    let dream_caps = cfg.caps.clone();
    let dream_providers = cfg.providers.clone();
    let dream_shutdown = Arc::clone(&shutdown);
    // M6: a second governed background loop runs a dream pass on an interval
    // (consolidate + decay), capped by dream_wallclock_secs + paid_spend_cap_usd.
    // The dream loop intentionally stays a direct low-frequency writer instead
    // of using the writer actor: consolidate_pending runs inference inside
    // Store methods, and parking that on the writer thread would serialize
    // capture latency behind dream passes (see writer.rs module docs).
    // The interval sleep is sliced so shutdown is observed within ~500ms; an
    // in-flight dream pass finishes before the thread exits.
    let dream_worker = std::thread::spawn(move || {
        let adapter = memoryd_core::adapters::AdapterKind::from_provider_config(&dream_providers);
        let mut dream_store = match Store::open(&dream_db) {
            Ok(store) => store,
            Err(err) => {
                logging::log_error!("dream store open failed: {err}");
                return;
            }
        };
        let opts = memoryd_core::dream::DreamOptions {
            trigger: "scheduled",
            budget_usd: dream_caps.paid_spend_cap_usd,
            max_seconds: dream_caps.dream_wallclock_secs,
        };
        'outer: loop {
            let mut slept = std::time::Duration::ZERO;
            let interval = std::time::Duration::from_secs(DREAM_INTERVAL_SECS);
            while slept < interval {
                if dream_shutdown.load(Ordering::Acquire) {
                    break 'outer;
                }
                let slice = std::time::Duration::from_millis(500).min(interval - slept);
                std::thread::sleep(slice);
                slept += slice;
            }
            if let Err(err) = memoryd_core::dream::dream_once(
                &mut dream_store,
                &adapter,
                &dream_caps,
                &opts,
                &|| unix_ms_now(),
            ) {
                logging::log_warn!("dream tick failed: {err}");
            }
        }
    });

    let listener = TcpListener::bind(cfg.bind)?;
    logging::log_info!("serve starting");
    logging::log_info!("bind: {}", cfg.bind);
    logging::log_info!("db_path: {}", cfg.db_path.display());
    logging::log_info!("worker: embed ({} adapter)", cfg.providers.default_adapter);
    logging::log_info!("dream: scheduled every {DREAM_INTERVAL_SECS}s");

    let result = serve_loop(
        listener,
        Arc::new(cfg),
        Arc::new(AuthThrottle::new()),
        writer.clone(),
        Arc::clone(&shutdown),
        HTTP_READ_TIMEOUT,
        HTTP_WRITE_TIMEOUT,
    );

    logging::log_info!("shutdown: draining background workers");
    if worker.join().is_err() {
        logging::log_error!("embed worker panicked during shutdown");
    }
    if dream_worker.join().is_err() {
        logging::log_error!("dream worker panicked during shutdown");
    }
    // Last writer handle drops here; the actor drains its queue and exits.
    drop(writer);
    if writer_thread.join().is_err() {
        logging::log_error!("store writer panicked during shutdown");
    }
    logging::log_info!("shutdown complete");
    result
}

/// Accept loop: each connection is handled on its own thread with its own
/// store, so one slow or stalled client cannot serialize other callers. The
/// active-connection counter bounds the thread count; peers past the cap get
/// an immediate 503. The listener is non-blocking so the shutdown flag is
/// observed within ~50ms; on shutdown the loop waits up to 5s for in-flight
/// connections to drain (socket timeouts bound their lifetime regardless).
fn serve_loop(
    listener: TcpListener,
    cfg: Arc<Config>,
    throttle: Arc<AuthThrottle>,
    writer: WriterHandle,
    shutdown: Arc<AtomicBool>,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<(), CliError> {
    listener.set_nonblocking(true)?;
    let active = Arc::new(AtomicUsize::new(0));
    while !shutdown.load(Ordering::Acquire) {
        match listener.accept() {
            Ok((mut stream, _)) => {
                // Hand the socket back to blocking mode; per-connection
                // deadlines come from read/write timeouts, not non-blocking IO.
                if stream.set_nonblocking(false).is_err() {
                    continue;
                }
                if active.load(Ordering::Acquire) >= MAX_CONCURRENT_CONNECTIONS {
                    let response =
                        HttpResponse::error(503, "server_busy", "too many concurrent connections");
                    let _ = stream.set_write_timeout(Some(write_timeout));
                    let _ = write_http_response(&mut stream, response);
                    continue;
                }
                let guard = ConnectionGuard::register(Arc::clone(&active));
                let cfg = Arc::clone(&cfg);
                let throttle = Arc::clone(&throttle);
                let writer = writer.clone();
                std::thread::spawn(move || {
                    let _guard = guard;
                    if let Err(err) = handle_connection_thread(
                        &cfg,
                        &throttle,
                        &writer,
                        stream,
                        read_timeout,
                        write_timeout,
                    ) {
                        logging::log_warn!("request failed: {err}");
                    }
                });
            }
            Err(err) if err.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(err) => return Err(CliError::Io(err)),
        }
    }

    let drain_deadline = std::time::Instant::now() + Duration::from_secs(5);
    while active.load(Ordering::Acquire) > 0 && std::time::Instant::now() < drain_deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Per-connection entry point: opens this thread's read-only store and
/// delegates; writes go through the writer actor. A store that cannot open
/// still produces an HTTP 500 instead of a silently dropped connection.
fn handle_connection_thread(
    cfg: &Config,
    throttle: &AuthThrottle,
    writer: &WriterHandle,
    mut stream: TcpStream,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<(), CliError> {
    match Store::open(&cfg.db_path) {
        Ok(store) => handle_http_connection(
            &store,
            writer,
            cfg,
            throttle,
            stream,
            read_timeout,
            write_timeout,
        ),
        Err(err) => {
            let _ = stream.set_write_timeout(Some(write_timeout));
            let _ = write_http_response(
                &mut stream,
                HttpResponse::error(500, "store_error", "store could not be opened"),
            );
            Err(CliError::Store(err))
        }
    }
}

/// RAII registration in the active-connection counter; decrements on drop so
/// panics and early returns cannot leak a slot.
struct ConnectionGuard {
    active: Arc<AtomicUsize>,
}

impl ConnectionGuard {
    fn register(active: Arc<AtomicUsize>) -> Self {
        active.fetch_add(1, Ordering::AcqRel);
        Self { active }
    }
}

impl Drop for ConnectionGuard {
    fn drop(&mut self) {
        self.active.fetch_sub(1, Ordering::AcqRel);
    }
}

/// Per-IP fixed-window auth throttle: `AUTH_FAIL_LIMIT` 401s within
/// `AUTH_FAIL_WINDOW_MS` lock the peer out for `AUTH_LOCKOUT_MS`. State is
/// in-memory only and bounded by `AUTH_THROTTLE_MAX_ENTRIES`. Callers inject
/// `now_ms` so tests control the clock.
struct AuthThrottle {
    inner: Mutex<HashMap<IpAddr, FailState>>,
}

#[derive(Debug, Clone, Copy)]
struct FailState {
    failures: u32,
    window_start_ms: i64,
    locked_until_ms: i64,
    last_failure_ms: i64,
}

impl AuthThrottle {
    fn new() -> Self {
        Self {
            inner: Mutex::new(HashMap::new()),
        }
    }

    fn is_throttled(&self, ip: IpAddr, now_ms: i64) -> bool {
        let map = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        map.get(&ip)
            .map(|state| state.locked_until_ms > now_ms)
            .unwrap_or(false)
    }

    fn record_failure(&self, ip: IpAddr, now_ms: i64) {
        let mut map = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        if !map.contains_key(&ip) && map.len() >= AUTH_THROTTLE_MAX_ENTRIES {
            Self::evict(&mut map, now_ms);
        }
        let state = map.entry(ip).or_insert(FailState {
            failures: 0,
            window_start_ms: now_ms,
            locked_until_ms: 0,
            last_failure_ms: now_ms,
        });
        if now_ms.saturating_sub(state.window_start_ms) > AUTH_FAIL_WINDOW_MS {
            state.failures = 0;
            state.window_start_ms = now_ms;
        }
        state.failures += 1;
        state.last_failure_ms = now_ms;
        if state.failures >= AUTH_FAIL_LIMIT {
            state.locked_until_ms = now_ms + AUTH_LOCKOUT_MS;
            state.failures = 0;
            state.window_start_ms = now_ms;
        }
    }

    fn record_success(&self, ip: IpAddr) {
        let mut map = self.inner.lock().unwrap_or_else(|err| err.into_inner());
        map.remove(&ip);
    }

    /// Drop entries that are neither locked out nor recently failing; if none
    /// expired, evict the entry with the oldest failure so the map stays bounded.
    fn evict(map: &mut HashMap<IpAddr, FailState>, now_ms: i64) {
        map.retain(|_, state| {
            state.locked_until_ms > now_ms
                || now_ms.saturating_sub(state.last_failure_ms) <= AUTH_FAIL_WINDOW_MS
        });
        if map.len() >= AUTH_THROTTLE_MAX_ENTRIES
            && let Some(oldest) = map
                .iter()
                .min_by_key(|(_, state)| state.last_failure_ms)
                .map(|(ip, _)| *ip)
        {
            map.remove(&oldest);
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum Command {
    Doctor,
    Stats,
    Serve,
    Remember(RememberArgs),
    Recall(RecallArgs),
    Import(ImportArgs),
    Dream(DreamArgs),
    Approve(ApproveArgs),
    Mcp,
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RememberArgs {
    content: String,
    kind: String,
    session_id: String,
    source: String,
    tags: Vec<String>,
    /// Capture surface stamped into sessions/audit provenance ("cli", "mcp").
    agent: String,
}

impl Default for RememberArgs {
    fn default() -> Self {
        Self {
            content: String::new(),
            kind: "note".to_string(),
            session_id: "cli".to_string(),
            source: "cli".to_string(),
            tags: Vec::new(),
            agent: "cli".to_string(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecallArgs {
    query: String,
    limit: usize,
    semantic: bool,
    /// One-hop graph expansion: 1 = expand over `memory_links` (default), 0 = direct only.
    hops: u8,
    /// Override the vector index for this recall ("brute-force" | "hnsw"); None = config default.
    index_kind: Option<String>,
}

impl Default for RecallArgs {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 5,
            semantic: false,
            index_kind: None,
            hops: 1,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ImportArgs {
    source: String,
    path: String,
}

impl Default for ImportArgs {
    fn default() -> Self {
        Self {
            source: "jsonl".to_string(),
            path: String::new(),
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct DreamArgs {
    budget_usd: Option<f64>,
    max_seconds: Option<u64>,
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ApproveArgs {
    list: bool,
    id: Option<String>,
    accept: bool,
    reject: bool,
}

#[derive(Debug, Clone)]
struct Cli {
    command: Command,
    db_path: PathBuf,
    bind: SocketAddr,
    bearer_token: Option<String>,
    /// Provider adapter override (null|local|openai_compat); also settable
    /// via MEMORYD_ADAPTER. Endpoint details come from MEMORYD_OPENAI_*.
    adapter: Option<String>,
}

impl Cli {
    fn parse(args: impl IntoIterator<Item = OsString>) -> Result<Self, CliError> {
        let mut args = args.into_iter();
        let _binary = args.next();
        let command = match args.next() {
            None => Command::Help,
            Some(raw) => match raw.to_string_lossy().as_ref() {
                "doctor" => Command::Doctor,
                "stats" => Command::Stats,
                "serve" => Command::Serve,
                "remember" => Command::Remember(RememberArgs::default()),
                "recall" => Command::Recall(RecallArgs::default()),
                "import" => Command::Import(ImportArgs::default()),
                "dream" => Command::Dream(DreamArgs::default()),
                "approve" => Command::Approve(ApproveArgs::default()),
                "mcp" => Command::Mcp,
                "--help" | "-h" | "help" => Command::Help,
                other => return Err(CliError::UnknownCommand(other.to_string())),
            },
        };
        let mut command = command;

        let mut db_path = default_db_path();
        let mut bind = DEFAULT_BIND
            .parse()
            .expect("DEFAULT_BIND must be a valid socket address");
        let mut bearer_token = env::var("MEMORYD_TOKEN").ok();
        let mut adapter: Option<String> = None;

        while let Some(raw) = args.next() {
            let token = raw.to_string_lossy().into_owned();
            match token.as_str() {
                "--db" => {
                    db_path = PathBuf::from(next_value(&mut args, "--db")?);
                }
                "--bind" => {
                    let value = next_string(&mut args, "--bind")?;
                    bind = value
                        .parse()
                        .map_err(|_| CliError::InvalidBind(value.to_string()))?;
                }
                "--token" => {
                    bearer_token = Some(next_string(&mut args, "--token")?);
                }
                "--adapter" => {
                    adapter = Some(next_string(&mut args, "--adapter")?);
                }
                "--token-file" => {
                    // Preferred over --token for real tokens: argv is
                    // world-readable via /proc/<pid>/cmdline, a file can be
                    // chmod 0600. Last of --token/--token-file wins; both
                    // override MEMORYD_TOKEN.
                    let path = next_string(&mut args, "--token-file")?;
                    let contents = std::fs::read_to_string(&path)
                        .map_err(|_| CliError::TokenFileUnreadable(path))?;
                    bearer_token = Some(contents.trim_end_matches(['\r', '\n']).to_string());
                }
                "--kind" => {
                    let Command::Remember(remember) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    remember.kind = next_string(&mut args, "--kind")?;
                }
                "--session" => {
                    let Command::Remember(remember) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    remember.session_id = next_string(&mut args, "--session")?;
                }
                "--source" => match &mut command {
                    Command::Remember(remember) => {
                        remember.source = next_string(&mut args, "--source")?;
                    }
                    Command::Import(import) => {
                        import.source = next_string(&mut args, "--source")?;
                    }
                    _ => return Err(CliError::UnknownFlag(token)),
                },
                "--path" => {
                    let Command::Import(import) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    import.path = next_string(&mut args, "--path")?;
                }
                "--budget-usd" => {
                    let Command::Dream(dream) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    let value = next_string(&mut args, "--budget-usd")?;
                    dream.budget_usd = Some(value.parse::<f64>().map_err(|_| {
                        CliError::InvalidNumberFlag("--budget-usd", value.to_string())
                    })?);
                }
                "--max-seconds" => {
                    let Command::Dream(dream) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    let value = next_string(&mut args, "--max-seconds")?;
                    let seconds = value.parse::<u64>().map_err(|_| {
                        CliError::InvalidNumberFlag("--max-seconds", value.to_string())
                    })?;
                    if seconds > memoryd_core::config::MAX_DURATION_SECS {
                        return Err(CliError::Config(
                            memoryd_core::config::ConfigError::CapDurationTooLarge {
                                field: "--max-seconds",
                                value: seconds,
                                max: memoryd_core::config::MAX_DURATION_SECS,
                            },
                        ));
                    }
                    dream.max_seconds = Some(seconds);
                }
                "--now" => {
                    if !matches!(command, Command::Dream(_)) {
                        return Err(CliError::UnknownFlag(token));
                    }
                }
                "--list" => {
                    let Command::Approve(approve) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    approve.list = true;
                }
                "--id" => {
                    let Command::Approve(approve) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    approve.id = Some(next_string(&mut args, "--id")?);
                }
                "--accept" => {
                    let Command::Approve(approve) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    approve.accept = true;
                }
                "--reject" => {
                    let Command::Approve(approve) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    approve.reject = true;
                }
                "--tags" => {
                    let Command::Remember(remember) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    remember.tags = parse_tags(&next_string(&mut args, "--tags")?);
                }
                "--k" => {
                    let Command::Recall(recall) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    let value = next_string(&mut args, "--k")?;
                    recall.limit = parse_limit("--k", &value)?;
                }
                "--semantic" => {
                    let Command::Recall(recall) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    recall.semantic = true;
                }
                "--hops" => {
                    let Command::Recall(recall) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    let value = next_string(&mut args, "--hops")?;
                    recall.hops = parse_hops(&value)?;
                }
                "--index" => {
                    let Command::Recall(recall) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    recall.index_kind = Some(next_string(&mut args, "--index")?);
                }
                "--no-wait" => {
                    if !matches!(command, Command::Remember(_)) {
                        return Err(CliError::UnknownFlag(token));
                    }
                }
                "--help" | "-h" => {
                    return Ok(Self {
                        command: Command::Help,
                        db_path,
                        bind,
                        bearer_token,
                        adapter,
                    });
                }
                other
                    if matches!(command, Command::Remember(_) | Command::Recall(_))
                        && !other.starts_with("--") =>
                {
                    let content = raw
                        .into_string()
                        .map_err(|_| CliError::InvalidUtf8Argument("argument"))?;
                    match &mut command {
                        Command::Remember(remember) => {
                            if !remember.content.is_empty() {
                                return Err(CliError::UnexpectedArgument(content));
                            }
                            remember.content = content;
                        }
                        Command::Recall(recall) => {
                            if !recall.query.is_empty() {
                                return Err(CliError::UnexpectedArgument(content));
                            }
                            recall.query = content;
                        }
                        _ => unreachable!("command checked above"),
                    }
                }
                other if other.starts_with("--") => return Err(CliError::UnknownFlag(token)),
                other => return Err(CliError::UnexpectedArgument(other.to_string())),
            }
        }

        if let Command::Remember(remember) = &command
            && remember.content.is_empty()
        {
            return Err(CliError::MissingArgument("content"));
        }
        if let Command::Recall(recall) = &command
            && recall.query.is_empty()
        {
            return Err(CliError::MissingArgument("query"));
        }

        Ok(Self {
            command,
            db_path,
            bind,
            bearer_token,
            adapter,
        })
    }

    fn config(&self) -> Result<Config, CliError> {
        let mut cfg = Config::with_db_path(self.db_path.clone());
        cfg.bind = self.bind;
        cfg.bearer_token = self.bearer_token.clone();
        cfg.apply_env()?;
        if let Some(adapter) = &self.adapter {
            cfg.providers.default_adapter = adapter.clone();
        }
        Ok(cfg)
    }
}

fn parse_tags(value: &str) -> Vec<String> {
    value
        .split(',')
        .map(str::trim)
        .filter(|tag| !tag.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn parse_limit(flag: &'static str, value: &str) -> Result<usize, CliError> {
    let limit = value
        .parse::<usize>()
        .map_err(|_| CliError::InvalidNumberFlag(flag, value.to_string()))?;
    if limit == 0 {
        return Err(CliError::InvalidNumberFlag(flag, value.to_string()));
    }
    Ok(limit)
}

/// Parse the `--hops` flag: only 0 (direct hits) or 1 (one-hop expansion) are valid.
fn parse_hops(value: &str) -> Result<u8, CliError> {
    match value {
        "0" => Ok(0),
        "1" => Ok(1),
        _ => Err(CliError::InvalidNumberFlag("--hops", value.to_string())),
    }
}

fn next_value(
    args: &mut impl Iterator<Item = OsString>,
    flag: &'static str,
) -> Result<OsString, CliError> {
    args.next().ok_or(CliError::MissingFlagValue(flag))
}

fn next_string(
    args: &mut impl Iterator<Item = OsString>,
    flag: &'static str,
) -> Result<String, CliError> {
    next_value(args, flag)?
        .into_string()
        .map_err(|_| CliError::InvalidUtf8FlagValue(flag))
}

fn default_db_path() -> PathBuf {
    if let Some(path) = env::var_os("MEMORYD_DB") {
        return PathBuf::from(path);
    }

    if let Some(path) = env::var_os("XDG_DATA_HOME") {
        return PathBuf::from(path).join("memoryd").join("memoryd.db");
    }

    if let Some(home) = env::var_os("HOME") {
        return PathBuf::from(home)
            .join(".local")
            .join("share")
            .join("memoryd")
            .join("memoryd.db");
    }

    PathBuf::from("memoryd.db")
}

fn print_help() {
    println!(
        "memoryd\n\n\
         Usage:\n\
            memoryd doctor [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>]\n\
            memoryd stats  [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>]\n\
            memoryd remember <content> [--kind <kind>] [--session <id>] [--source <source>] [--tags <a,b>] [--db <path>]\n\
            memoryd recall <query> [--k <limit>] [--semantic] [--hops <0|1>] [--index <brute-force|hnsw>] [--db <path>]\n\
            memoryd import --source jsonl --path <file> [--db <path>]\n\
            memoryd dream [--now] [--budget-usd <n>] [--max-seconds <n>] [--db <path>]\n\
            memoryd approve [--list] [--id <id> --accept|--reject] [--db <path>]\n\
            memoryd mcp [--db <path>]   (MCP stdio server; no network bind)\n\
            memoryd serve [--db <path>] [--bind <addr:port>] [--token <token>] [--token-file <path>] [--adapter <null|local|openai_compat>]\n\n\
          Provider env: MEMORYD_ADAPTER, MEMORYD_SPEND_CAP_USD, MEMORYD_OPENAI_BASE_URL,\n\
          MEMORYD_OPENAI_API_KEY[_FILE], MEMORYD_OPENAI_EMBED_MODEL, MEMORYD_OPENAI_CHAT_MODEL,\n\
          MEMORYD_OPENAI_USD_PER_1K.\n\n\
          Tokens: prefer MEMORYD_TOKEN or --token-file over --token; command-line\n\
          arguments are world-readable via /proc/<pid>/cmdline.\n\n\
          Defaults:\n\
            bind: {DEFAULT_BIND}\n\
            provider: null\n\
           paid spend cap: 0.00"
    );
}

#[derive(Debug)]
enum CliError {
    UnknownCommand(String),
    UnknownFlag(String),
    UnexpectedArgument(String),
    MissingArgument(&'static str),
    MissingFlagValue(&'static str),
    InvalidUtf8FlagValue(&'static str),
    InvalidUtf8Argument(&'static str),
    InvalidNumberFlag(&'static str, String),
    InvalidBind(String),
    TokenFileUnreadable(String),
    Config(memoryd_core::config::ConfigError),
    Store(memoryd_core::store::StoreError),
    Json(serde_json::Error),
    Io(std::io::Error),
    DoctorFailed,
}

impl fmt::Display for CliError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::UnknownCommand(command) => write!(f, "unknown command: {command}"),
            Self::UnknownFlag(flag) => write!(f, "unknown flag: {flag}"),
            Self::UnexpectedArgument(argument) => write!(f, "unexpected argument: {argument}"),
            Self::MissingArgument(argument) => write!(f, "missing argument: {argument}"),
            Self::MissingFlagValue(flag) => write!(f, "missing value for {flag}"),
            Self::InvalidUtf8FlagValue(flag) => write!(f, "value for {flag} must be valid UTF-8"),
            Self::InvalidUtf8Argument(argument) => {
                write!(f, "argument {argument} must be valid UTF-8")
            }
            Self::InvalidNumberFlag(flag, value) => {
                write!(f, "value for {flag} must be a positive integer: {value}")
            }
            Self::InvalidBind(bind) => write!(f, "invalid bind address: {bind}"),
            Self::TokenFileUnreadable(path) => {
                write!(f, "could not read token file {path}")
            }
            Self::Config(err) => write!(f, "configuration error: {err}"),
            Self::Store(err) => write!(f, "{err}"),
            Self::Json(err) => write!(f, "JSON error: {err}"),
            Self::Io(err) => write!(f, "I/O error: {err}"),
            Self::DoctorFailed => write!(f, "doctor checks failed"),
        }
    }
}

impl std::error::Error for CliError {}

impl From<memoryd_core::config::ConfigError> for CliError {
    fn from(err: memoryd_core::config::ConfigError) -> Self {
        Self::Config(err)
    }
}

impl From<memoryd_core::store::StoreError> for CliError {
    fn from(err: memoryd_core::store::StoreError) -> Self {
        Self::Store(err)
    }
}

impl From<serde_json::Error> for CliError {
    fn from(err: serde_json::Error) -> Self {
        Self::Json(err)
    }
}

impl From<std::io::Error> for CliError {
    fn from(err: std::io::Error) -> Self {
        Self::Io(err)
    }
}

fn remember_event(args: RememberArgs) -> NewRawEvent {
    NewRawEvent {
        session_id: args.session_id,
        agent: args.agent,
        source: args.source,
        kind: "memory".to_string(),
        payload: serde_json::json!({
            "text": args.content,
            "memory_kind": args.kind.clone(),
        }),
        provenance: serde_json::json!({
            "via": "remember",
            "memory_kind": args.kind,
            "tags": args.tags,
        }),
        ts_ms: unix_ms_now(),
    }
}

fn dream_response_json(outcome: &memoryd_core::dream::DreamOutcome) -> Result<String, CliError> {
    Ok(serde_json::to_string(&serde_json::json!({
        "run_id": outcome.run_id,
        "consolidated": outcome.consolidated,
        "distilled": outcome.distilled,
        "associated": outcome.associated,
        "proposed": outcome.proposed,
        "decayed": outcome.decayed,
        "tokens_used": outcome.tokens_used,
        "status": outcome.status,
    }))?)
}

fn import_response_json(summary: &memoryd_core::import::ImportSummary) -> Result<String, CliError> {
    Ok(serde_json::to_string(&serde_json::json!({
        "batch_id": summary.batch_id,
        "source": summary.source,
        "path": summary.path,
        "total": summary.total,
        "processed": summary.processed,
        "skipped": summary.skipped,
        "state": summary.state,
    }))?)
}

fn remember_response_json(ack: &CaptureAck) -> Result<String, CliError> {
    Ok(serde_json::to_string(&serde_json::json!({
        "raw_event_id": ack.raw_event_id,
        "session_id": ack.session_id,
        "enqueued_job_id": ack.enqueued_job_id,
        "pending_memory": ack.enqueued_job_id.is_some(),
        "degraded": ack.degraded,
    }))?)
}

fn recall_response_json(result: &RecallOutput) -> Result<String, CliError> {
    Ok(serde_json::to_string(&recall_response_value(result))?)
}

fn recall_response_value(result: &RecallOutput) -> serde_json::Value {
    match result {
        RecallOutput::Memory(memory) => {
            let hits = memory
                .hits
                .iter()
                .map(|hit| {
                    serde_json::json!({
                        "memory_id": hit.memory_id,
                        "kind": hit.kind,
                        "content": hit.content,
                        "score": hit.score,
                        "via_hop": hit.via_hop,
                        "link_strength": hit.link_strength,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "results": hits,
                "degraded": memory.degraded,
                "mode": memory.mode,
                "compared": memory.compared,
            })
        }
        RecallOutput::Event(event) => {
            let hits = event
                .hits
                .iter()
                .map(|hit| {
                    serde_json::json!({
                        "raw_event_id": hit.raw_event_id,
                        "session_id": hit.session_id,
                        "ts_ms": hit.ts_ms,
                        "source": hit.source,
                        "kind": hit.kind,
                        "content": hit.content,
                        "score": hit.score,
                    })
                })
                .collect::<Vec<_>>();
            serde_json::json!({
                "results": hits,
                "degraded": event.degraded,
                "mode": event.mode,
                "compared": event.compared,
            })
        }
    }
}

fn handle_http_connection(
    store: &Store,
    writer: &WriterHandle,
    cfg: &Config,
    throttle: &AuthThrottle,
    mut stream: TcpStream,
    read_timeout: Duration,
    write_timeout: Duration,
) -> Result<(), CliError> {
    stream.set_read_timeout(Some(read_timeout))?;
    stream.set_write_timeout(Some(write_timeout))?;
    let peer = stream.peer_addr().ok();
    let peer_ip = peer.map(|addr| addr.ip());

    // Throttled peers are rejected before any request byte is read: cheapest
    // possible path for a brute-forcing client, and no parser work.
    if let Some(ip) = peer_ip
        && throttle.is_throttled(ip, unix_ms_now())
    {
        write_http_response(
            &mut stream,
            HttpResponse::error(
                429,
                "rate_limited",
                "too many failed authentication attempts; retry later",
            ),
        )?;
        return Ok(());
    }

    let response = match read_http_request(&mut stream) {
        Ok(request) => handle_http_request(store, writer, cfg, peer, request),
        Err(err) => HttpResponse::error(err.status, err.code, err.message),
    };
    let status = response.status;
    write_http_response(&mut stream, response)?;
    if let Some(ip) = peer_ip {
        if status == 401 {
            throttle.record_failure(ip, unix_ms_now());
        } else if (200..300).contains(&status) {
            throttle.record_success(ip);
        }
    }
    Ok(())
}

fn handle_http_request(
    store: &Store,
    writer: &WriterHandle,
    cfg: &Config,
    peer: Option<SocketAddr>,
    request: HttpRequest,
) -> HttpResponse {
    // GET /v1/health is auth-exempt for loopback peers only: it is read-only
    // and leaks nothing beyond the schema version, so local supervisors can
    // probe liveness without the bearer token. Remote probes still need auth.
    if request.path == "/v1/health" && peer.map(|addr| addr.ip().is_loopback()).unwrap_or(false) {
        return handle_http_health(store, &request);
    }

    if !is_authorized(cfg, peer, &request.headers) {
        let peer_loopback = peer.map(|addr| addr.ip().is_loopback());
        let authorization_header_present =
            header_value(&request.headers, "authorization").is_some();
        let method = request.method.clone();
        let path = request.path.clone();
        let reason = auth_rejection_reason(cfg, peer);
        let audited = writer.exec(move |s| {
            s.record_auth_rejection(
                &method,
                &path,
                peer_loopback,
                authorization_header_present,
                reason,
            )
        });
        if !matches!(audited, Ok(Ok(()))) {
            return HttpResponse::error(500, "store_error", "auth audit could not be persisted");
        }
        return HttpResponse::error(401, "unauthorized", "authorization failed");
    }

    // The parser only supports Content-Length framing; a chunked request would
    // otherwise parse as a zero-length body and return a misleading JSON error.
    if header_value(&request.headers, "transfer-encoding")
        .map(|value| value.to_ascii_lowercase().contains("chunked"))
        .unwrap_or(false)
    {
        return HttpResponse::error(
            501,
            "not_implemented",
            "Transfer-Encoding is not supported; send Content-Length",
        );
    }

    // Authorized non-loopback callers reach health here.
    if request.path == "/v1/health" {
        return handle_http_health(store, &request);
    }

    if request.path != "/v1/capture" && request.path != "/v1/recall" {
        return HttpResponse::error(404, "not_found", "route not found");
    }

    if request.method != "POST" {
        return HttpResponse::error(405, "method_not_allowed", "POST required");
    }

    if !has_json_content_type(&request.headers) {
        return HttpResponse::error(415, "unsupported_media_type", "application/json required");
    }

    let body = match serde_json::from_slice::<serde_json::Value>(&request.body) {
        Ok(body) => body,
        Err(_) => return HttpResponse::error(400, "invalid_json", "request body must be JSON"),
    };
    match request.path.as_str() {
        "/v1/capture" => handle_http_capture(writer, body, cfg.caps.queue_depth_max),
        "/v1/recall" => handle_http_recall(store, body, &cfg.providers),
        _ => HttpResponse::error(404, "not_found", "route not found"),
    }
}

fn handle_http_health(store: &Store, request: &HttpRequest) -> HttpResponse {
    if request.method != "GET" {
        return HttpResponse::error(405, "method_not_allowed", "GET required");
    }
    match store.schema_version() {
        Ok(schema_version) => HttpResponse::json(
            200,
            "OK",
            serde_json::json!({
                "status": "ok",
                "schema_version": schema_version,
            }),
        ),
        Err(_) => HttpResponse::error(500, "store_error", "health check could not read the store"),
    }
}

fn handle_http_capture(
    writer: &WriterHandle,
    body: serde_json::Value,
    max_active_jobs: usize,
) -> HttpResponse {
    let event = match capture_event_from_json(body) {
        Ok(event) => event,
        Err(message) => return HttpResponse::error(422, "invalid_request", message),
    };

    match writer.exec(move |s| s.capture_event_with_queue_limit(event, max_active_jobs)) {
        Ok(Ok(ack)) => HttpResponse::json(
            202,
            "Accepted",
            serde_json::json!({
                "raw_event_id": ack.raw_event_id,
                "session_id": ack.session_id,
                "enqueued_job_id": ack.enqueued_job_id,
                "degraded": ack.degraded,
                "processed": ack.processed,
            }),
        ),
        Ok(Err(StoreError::InvalidCaptureField(_))) => {
            HttpResponse::error(422, "invalid_request", "capture fields must not be empty")
        }
        Ok(Err(_)) | Err(_) => {
            HttpResponse::error(500, "store_error", "capture could not be persisted")
        }
    }
}

fn handle_http_recall(
    store: &Store,
    body: serde_json::Value,
    providers: &memoryd_core::config::ProviderConfig,
) -> HttpResponse {
    let args = match recall_request_from_json(body) {
        Ok(args) => args,
        Err(message) => return HttpResponse::error(422, "invalid_request", message),
    };

    // HTTP recall always uses the brute-force oracle: the `--index` selector is a
    // CLI-only override, and HNSW is not yet a latency win (it builds per call over the
    // shortlist — see vectorindex.rs / ARCHITECTURE-PLAN §21.12). Revisit when the
    // persistent full-corpus index lands.
    let adapter = memoryd_core::adapters::AdapterKind::from_provider_config(providers);
    match recall_with_mode(store, &args, "brute-force", &adapter) {
        Ok(result) => HttpResponse::json(200, "OK", recall_response_value(&result)),
        Err(StoreError::InvalidRecallQuery) => {
            HttpResponse::error(422, "invalid_request", "query must contain searchable text")
        }
        Err(_) => HttpResponse::error(500, "store_error", "recall could not be completed"),
    }
}

fn read_http_request(stream: &mut TcpStream) -> Result<HttpRequest, HttpParseError> {
    let request_line = read_http_line(stream)?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| HttpParseError::new(400, "bad_request", "missing HTTP request method"))?;
    let path = parts
        .next()
        .ok_or_else(|| HttpParseError::new(400, "bad_request", "missing HTTP request path"))?;
    let version = parts
        .next()
        .ok_or_else(|| HttpParseError::new(400, "bad_request", "missing HTTP request version"))?;
    if !version.starts_with("HTTP/") {
        return Err(HttpParseError::new(
            400,
            "bad_request",
            "invalid HTTP request version",
        ));
    }

    let mut headers = Vec::new();
    let mut content_length = None;
    for _ in 0..MAX_HTTP_HEADERS {
        let line = read_http_line(stream)?;
        if line.is_empty() {
            let body_len = content_length.unwrap_or(0);
            if body_len > MAX_HTTP_BODY_BYTES {
                return Err(HttpParseError::new(
                    413,
                    "body_too_large",
                    "request body exceeds limit",
                ));
            }
            let mut body = vec![0; body_len];
            stream.read_exact(&mut body).map_err(|err| {
                if is_timeout(&err) {
                    http_timeout_error()
                } else {
                    HttpParseError::new(400, "bad_request", "could not read request body")
                }
            })?;
            return Ok(HttpRequest {
                method: method.to_string(),
                path: path.to_string(),
                headers,
                body,
            });
        }

        let Some((name, value)) = line.split_once(':') else {
            return Err(HttpParseError::new(
                400,
                "bad_request",
                "malformed HTTP header",
            ));
        };
        let name = name.trim().to_ascii_lowercase();
        let value = value.trim().to_string();
        if name == "content-length" {
            content_length = Some(value.parse::<usize>().map_err(|_| {
                HttpParseError::new(400, "bad_request", "invalid content-length header")
            })?);
        }
        headers.push((name, value));
    }

    Err(HttpParseError::new(
        431,
        "headers_too_large",
        "too many HTTP headers",
    ))
}

fn read_http_line(stream: &mut TcpStream) -> Result<String, HttpParseError> {
    let mut bytes = Vec::new();
    let mut byte = [0_u8; 1];
    while bytes.len() <= MAX_HTTP_LINE_BYTES {
        match stream.read(&mut byte) {
            Ok(0) if bytes.is_empty() => {
                return Err(HttpParseError::new(
                    400,
                    "bad_request",
                    "empty HTTP request",
                ));
            }
            Ok(0) => break,
            Ok(_) => {
                bytes.push(byte[0]);
                if byte[0] == b'\n' {
                    break;
                }
            }
            Err(err) if is_timeout(&err) => return Err(http_timeout_error()),
            Err(_) => {
                return Err(HttpParseError::new(
                    400,
                    "bad_request",
                    "could not read HTTP request",
                ));
            }
        }
    }

    if bytes.len() > MAX_HTTP_LINE_BYTES {
        return Err(HttpParseError::new(
            431,
            "headers_too_large",
            "HTTP line exceeds limit",
        ));
    }

    let line = String::from_utf8(bytes)
        .map_err(|_| HttpParseError::new(400, "bad_request", "HTTP request must be UTF-8"))?;
    Ok(line.trim_end_matches(['\r', '\n']).to_string())
}

/// Socket read timeouts surface as `WouldBlock` or `TimedOut` depending on the
/// platform; both mean the client did not deliver bytes within the deadline.
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

fn http_timeout_error() -> HttpParseError {
    HttpParseError::new(
        408,
        "request_timeout",
        "client did not send a complete request in time",
    )
}

fn capture_event_from_json(value: serde_json::Value) -> Result<NewRawEvent, &'static str> {
    let object = value
        .as_object()
        .ok_or("request body must be a JSON object")?;
    let session_id = required_json_string(object, "session_id")?;
    let agent = required_json_string(object, "agent")?;
    let source = required_json_string(object, "source")?;
    let kind = required_json_string(object, "kind")?;
    let payload = object
        .get("payload")
        .cloned()
        .ok_or("payload is required")?;
    let provenance = object
        .get("provenance")
        .cloned()
        .unwrap_or_else(|| serde_json::json!({}));
    let ts_ms = match object.get("ts_ms").or_else(|| object.get("ts")) {
        Some(value) if value.is_number() => value.as_i64().ok_or("timestamp is out of range")?,
        Some(_) => return Err("ts_ms must be integer milliseconds"),
        None => unix_ms_now(),
    };

    Ok(NewRawEvent {
        session_id,
        agent,
        source,
        kind,
        payload,
        provenance,
        ts_ms,
    })
}

fn recall_request_from_json(value: serde_json::Value) -> Result<RecallArgs, &'static str> {
    let object = value
        .as_object()
        .ok_or("request body must be a JSON object")?;
    let query = required_json_string(object, "query")?;
    let limit = match object.get("k").or_else(|| object.get("limit")) {
        Some(value) if value.is_u64() => {
            usize::try_from(value.as_u64().ok_or("k is out of range")?)
                .map_err(|_| "k is out of range")?
        }
        Some(_) => return Err("k must be a positive integer"),
        None => 5,
    };
    if limit == 0 {
        return Err("k must be a positive integer");
    }
    let semantic = match object.get("semantic") {
        Some(value) => value.as_bool().ok_or("semantic must be a boolean")?,
        None => false,
    };
    let hops = match object.get("hops") {
        Some(value) if value.is_u64() => match value.as_u64() {
            Some(0) => 0,
            Some(1) => 1,
            _ => return Err("hops must be 0 or 1"),
        },
        Some(_) => return Err("hops must be 0 or 1"),
        None => 1,
    };

    Ok(RecallArgs {
        query,
        limit,
        semantic,
        hops,
        index_kind: None,
    })
}

fn required_json_string(
    object: &serde_json::Map<String, serde_json::Value>,
    field: &'static str,
) -> Result<String, &'static str> {
    object
        .get(field)
        .and_then(serde_json::Value::as_str)
        .map(ToOwned::to_owned)
        .ok_or(match field {
            "session_id" => "session_id is required",
            "agent" => "agent is required",
            "source" => "source is required",
            "kind" => "kind is required",
            "query" => "query is required",
            _ => "required string field is missing",
        })
}

fn has_json_content_type(headers: &[(String, String)]) -> bool {
    header_value(headers, "content-type")
        .map(|value| value.split(';').next().unwrap_or("").trim() == "application/json")
        .unwrap_or(false)
}

fn is_authorized(cfg: &Config, peer: Option<SocketAddr>, headers: &[(String, String)]) -> bool {
    if let Some(token) = cfg.bearer_token.as_deref() {
        let expected = format!("Bearer {token}");
        return header_value(headers, "authorization")
            .map(|actual| constant_time_eq(actual.as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
    }

    peer.map(|addr| addr.ip().is_loopback()).unwrap_or(false)
}

fn auth_rejection_reason(cfg: &Config, peer: Option<SocketAddr>) -> &'static str {
    if cfg.bearer_token.is_some() {
        "missing_or_invalid_bearer"
    } else if peer.is_some() {
        "non_loopback_peer"
    } else {
        "unknown_peer"
    }
}

fn header_value<'a>(headers: &'a [(String, String)], name: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(header_name, _)| header_name == name)
        .map(|(_, value)| value.as_str())
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    let mut diff = left.len() ^ right.len();
    let max_len = left.len().max(right.len());
    for index in 0..max_len {
        diff |= usize::from(
            left.get(index).copied().unwrap_or(0) ^ right.get(index).copied().unwrap_or(0),
        );
    }
    diff == 0
}

fn write_http_response(stream: &mut TcpStream, response: HttpResponse) -> Result<(), CliError> {
    let body = serde_json::to_vec(&response.body)?;
    write!(
        stream,
        "HTTP/1.1 {} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        response.status,
        response.reason,
        body.len()
    )?;
    stream.write_all(&body)?;
    Ok(())
}

fn unix_ms_now() -> i64 {
    let millis = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis();
    i64::try_from(millis).unwrap_or(i64::MAX)
}

struct HttpRequest {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

struct HttpResponse {
    status: u16,
    reason: &'static str,
    body: serde_json::Value,
}

impl HttpResponse {
    fn json(status: u16, reason: &'static str, body: serde_json::Value) -> Self {
        Self {
            status,
            reason,
            body,
        }
    }

    fn error(status: u16, code: &'static str, message: &'static str) -> Self {
        let reason = match status {
            400 => "Bad Request",
            401 => "Unauthorized",
            404 => "Not Found",
            405 => "Method Not Allowed",
            408 => "Request Timeout",
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            422 => "Unprocessable Entity",
            429 => "Too Many Requests",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
            503 => "Service Unavailable",
            _ => "Error",
        };
        Self::json(
            status,
            reason,
            serde_json::json!({
                "error": {
                    "code": code,
                    "message": message,
                }
            }),
        )
    }
}

struct HttpParseError {
    status: u16,
    code: &'static str,
    message: &'static str,
}

impl HttpParseError {
    fn new(status: u16, code: &'static str, message: &'static str) -> Self {
        Self {
            status,
            code,
            message,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use memoryd_core::store::TableStats;
    use std::fs;
    use std::path::Path;
    use std::time::{Duration, Instant};

    #[test]
    fn parses_doctor_with_db_path() {
        let cli =
            Cli::parse(["memoryd", "doctor", "--db", "/tmp/memoryd-test.db"].map(OsString::from))
                .expect("cli parses");

        assert_eq!(cli.command, Command::Doctor);
        assert_eq!(cli.db_path, PathBuf::from("/tmp/memoryd-test.db"));
    }

    #[test]
    fn parses_remember_with_kind_session_source_and_tags() {
        let cli = Cli::parse(
            [
                "memoryd",
                "remember",
                "Prod migrations use flyway",
                "--kind",
                "rule",
                "--session",
                "session-1",
                "--source",
                "hook",
                "--tags",
                "ops,db",
            ]
            .map(OsString::from),
        )
        .expect("cli parses");

        let Command::Remember(args) = cli.command else {
            panic!("expected remember command");
        };
        assert_eq!(args.content, "Prod migrations use flyway");
        assert_eq!(args.kind, "rule");
        assert_eq!(args.session_id, "session-1");
        assert_eq!(args.source, "hook");
        assert_eq!(args.tags, ["ops", "db"]);
    }

    #[test]
    fn parses_serve_command() {
        let cli = Cli::parse(["memoryd", "serve"].map(OsString::from)).expect("cli parses");

        assert_eq!(cli.command, Command::Serve);
    }

    #[test]
    fn parses_token_file_flag() {
        let token_path = temp_db_path("token-file").with_extension("token");
        fs::write(&token_path, "file-token-0123456789\n").expect("token file written");

        let cli = Cli::parse(
            [
                "memoryd",
                "serve",
                "--token-file",
                token_path.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");

        assert_eq!(
            cli.bearer_token.as_deref(),
            Some("file-token-0123456789"),
            "token read from file with trailing newline trimmed"
        );
        let _ = fs::remove_file(&token_path);
    }

    #[test]
    fn token_file_missing_is_an_error() {
        let missing = temp_db_path("token-file-missing").with_extension("absent");
        let result = Cli::parse(
            [
                "memoryd",
                "serve",
                "--token-file",
                missing.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        );
        assert!(matches!(result, Err(CliError::TokenFileUnreadable(_))));
    }

    #[test]
    fn parses_recall_with_limit() {
        let cli = Cli::parse(["memoryd", "recall", "wal timeout", "--k", "3"].map(OsString::from))
            .expect("cli parses");

        let Command::Recall(args) = cli.command else {
            panic!("expected recall command");
        };
        assert_eq!(args.query, "wal timeout");
        assert_eq!(args.limit, 3);
    }

    #[test]
    fn remember_command_persists_memory_capture() {
        let path = temp_db_path("remember-command");
        let cli = Cli::parse(
            [
                "memoryd",
                "remember",
                "Prod migrations use flyway",
                "--kind",
                "rule",
                "--tags",
                "ops,db",
                "--db",
                path.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Remember(args) = cli.command.clone() else {
            panic!("expected remember command");
        };

        remember(cli, args).expect("remember succeeds");

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "sessions"), 1);
        assert_eq!(table_rows(&stats, "raw_events"), 1);
        assert_eq!(table_rows(&stats, "jobs"), 1);
        assert_eq!(table_rows(&stats, "provider_usage"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn remember_command_redacts_secret_text_before_recall_indexing() {
        let path = temp_db_path("remember-redacts-before-recall");
        let bearer = "leakycredentialvalue";
        let email = "ops@example.test";
        let content = format!("Deploy with Authorization: Bearer {bearer}; contact {email}");
        let cli = Cli::parse(
            [
                "memoryd",
                "remember",
                content.as_str(),
                "--db",
                path.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Remember(args) = cli.command.clone() else {
            panic!("expected remember command");
        };

        remember(cli, args).expect("remember succeeds");

        let store = Store::open(&path).expect("store opens");
        let result = store.recall_events("redacted", 5).expect("recall succeeds");
        assert_eq!(result.hits.len(), 1);
        assert_eq!(
            result.hits[0].content,
            "Deploy with Authorization: Bearer [REDACTED]; contact [REDACTED]"
        );
        assert!(!result.hits[0].content.contains(bearer));
        assert!(!result.hits[0].content.contains(email));
        assert_eq!(
            store
                .recall_events("leakycredentialvalue", 5)
                .expect("secret recall succeeds")
                .hits
                .len(),
            0
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn recall_command_reads_lexical_capture_without_provider_usage() {
        let path = temp_db_path("recall-command");
        {
            let mut store = Store::open(&path).expect("store opens");
            store
                .capture_event(NewRawEvent {
                    session_id: "session-1".to_string(),
                    agent: "claude".to_string(),
                    source: "tool_result".to_string(),
                    kind: "observation".to_string(),
                    payload: serde_json::json!({"text": "WAL timeout fixed"}),
                    provenance: serde_json::json!({}),
                    ts_ms: 1234,
                })
                .expect("capture succeeds");
        }
        let cli = Cli::parse(
            [
                "memoryd",
                "recall",
                "wal timeout",
                "--k",
                "5",
                "--db",
                path.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Recall(args) = cli.command.clone() else {
            panic!("expected recall command");
        };

        recall(cli, args).expect("recall succeeds");

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "provider_usage"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn parses_recall_with_semantic_flag() {
        let cli =
            Cli::parse(["memoryd", "recall", "wal timeout", "--semantic"].map(OsString::from))
                .expect("cli parses");

        let Command::Recall(args) = cli.command else {
            panic!("expected recall command");
        };
        assert_eq!(args.query, "wal timeout");
        assert!(args.semantic, "--semantic sets the semantic flag");
    }

    #[test]
    fn recall_with_mode_semantic_degrades_to_lexical_under_null() {
        let path = temp_db_path("recall-with-mode-degrade");
        {
            let mut store = Store::open(&path).expect("store opens");
            store
                .capture_event(NewRawEvent {
                    session_id: "session-1".to_string(),
                    agent: "claude".to_string(),
                    source: "tool_result".to_string(),
                    kind: "observation".to_string(),
                    payload: serde_json::json!({"text": "WAL timeout fixed"}),
                    provenance: serde_json::json!({}),
                    ts_ms: 1234,
                })
                .expect("capture succeeds");
        }
        let store = Store::open(&path).expect("store opens");

        let args = RecallArgs {
            query: "wal timeout".to_string(),
            limit: 5,
            semantic: true,
            hops: 1,
            index_kind: None,
        };
        let result = recall_with_mode(
            &store,
            &args,
            "brute-force",
            &memoryd_core::adapters::AdapterKind::from_default_adapter("null"),
        )
        .expect("recall succeeds");

        // No memory exists (no dream run), so recall falls back to raw-event recall.
        // The only shipped adapter is null, which self-degrades
        // (embeds_semantically=false), so `--semantic` returns lexical-shaped results
        // flagged degraded — no provider spend, no query embedding cached.
        let RecallOutput::Event(result) = result else {
            panic!("expected raw-event fallback when no memory matches");
        };
        assert_eq!(result.mode, "lexical");
        assert!(result.degraded);
        assert_eq!(result.compared, 0);
        assert!(
            !result.hits.is_empty(),
            "lexical fallback still finds the match"
        );

        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "provider_usage"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn parses_import_with_source_and_path() {
        let cli = Cli::parse(
            [
                "memoryd",
                "import",
                "--source",
                "jsonl",
                "--path",
                "/tmp/hist.jsonl",
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Import(args) = cli.command else {
            panic!("expected import command");
        };
        assert_eq!(args.source, "jsonl");
        assert_eq!(args.path, "/tmp/hist.jsonl");
    }

    #[test]
    fn import_command_stages_jsonl_through_capture_path() {
        let db = temp_db_path("import-command");
        let src =
            std::env::temp_dir().join(format!("memoryd-import-cmd-{}.jsonl", std::process::id()));
        fs::write(
            &src,
            "{\"text\":\"flyway runs migrations\",\"ts_ms\":1}\n\
             {\"text\":\"wal checkpoint tuning\",\"ts_ms\":2}\n",
        )
        .expect("write jsonl fixture");

        let cli = Cli::parse(
            [
                "memoryd",
                "import",
                "--source",
                "jsonl",
                "--path",
                src.to_str().expect("path is UTF-8"),
                "--db",
                db.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Import(args) = cli.command.clone() else {
            panic!("expected import command");
        };

        import(cli, args).expect("import succeeds");

        let store = Store::open(&db).expect("store opens");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 2);
        assert_eq!(table_rows(&stats, "jobs"), 2);
        assert_eq!(table_rows(&stats, "import_batches"), 1);

        let _ = fs::remove_file(&src);
        cleanup_db_files(&db);
    }

    #[test]
    fn parses_dream_with_flags() {
        let cli = Cli::parse(
            [
                "memoryd",
                "dream",
                "--now",
                "--budget-usd",
                "2.5",
                "--max-seconds",
                "30",
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Dream(args) = cli.command else {
            panic!("expected dream command");
        };
        assert_eq!(args.budget_usd, Some(2.5));
        assert_eq!(args.max_seconds, Some(30));
    }

    #[test]
    fn dream_command_consolidates_captured_events() {
        let path = temp_db_path("dream-command");
        {
            let mut store = Store::open(&path).expect("store opens");
            for (i, text) in ["wal fix", "wal fix", "vacuum schedule"].iter().enumerate() {
                store
                    .capture_event(NewRawEvent {
                        session_id: "s1".to_string(),
                        agent: "claude".to_string(),
                        source: "tool_result".to_string(),
                        kind: "observation".to_string(),
                        payload: serde_json::json!({ "text": text }),
                        provenance: serde_json::json!({}),
                        ts_ms: 1000 + i as i64,
                    })
                    .expect("capture succeeds");
            }
        }
        let cli = Cli::parse(
            [
                "memoryd",
                "dream",
                "--now",
                "--db",
                path.to_str().expect("path is UTF-8"),
            ]
            .map(OsString::from),
        )
        .expect("cli parses");
        let Command::Dream(args) = cli.command.clone() else {
            panic!("expected dream command");
        };

        dream(cli, args).expect("dream succeeds");

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(
            table_rows(&stats, "memories"),
            2,
            "duplicate texts dedup to two memories"
        );
        assert_eq!(table_rows(&stats, "dream_runs"), 1);
        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_route_persists_event_on_loopback() {
        let path = temp_db_path("http-capture");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"WAL timeout fixed"},
                    "provenance":{"tags":["db"]},
                    "ts_ms":1234
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 202);
        assert_eq!(response.body["raw_event_id"], 1);
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "sessions"), 1);
        assert_eq!(table_rows(&stats, "raw_events"), 1);
        assert_eq!(table_rows(&stats, "jobs"), 1);
        assert_eq!(table_rows(&stats, "provider_usage"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_degrades_instead_of_failing_when_queue_is_full() {
        let path = temp_db_path("http-capture-queue-full");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.caps.queue_depth_max = 0;
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"WAL timeout fixed"},
                    "ts_ms":1234
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 202);
        assert_eq!(response.body["raw_event_id"], 1);
        assert_eq!(response.body["enqueued_job_id"], serde_json::Value::Null);
        assert_eq!(response.body["degraded"], true);
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 1);
        assert_eq!(table_rows(&stats, "sessions"), 1);
        assert_eq!(table_rows(&stats, "jobs"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    #[ignore = "performance evidence fixture; run explicitly on an idle host"]
    fn http_capture_100_sequential_requests_p95_stays_under_m1_target() {
        let path = temp_db_path("http-capture-latency");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);
        let mut durations = Vec::with_capacity(100);

        for index in 0..100 {
            let body = format!(
                r#"{{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{{"text":"WAL timeout fixed {index}"}},
                    "ts_ms":{}
                }}"#,
                1_000 + index
            )
            .into_bytes();
            let started = Instant::now();
            let response = handle_http_request(
                &store,
                &writer,
                &cfg,
                Some("127.0.0.1:65000".parse().expect("peer parses")),
                HttpRequest {
                    method: "POST".to_string(),
                    path: "/v1/capture".to_string(),
                    headers: vec![("content-type".to_string(), "application/json".to_string())],
                    body,
                },
            );
            durations.push(started.elapsed());
            assert_eq!(response.status, 202);
            assert_eq!(response.body["degraded"], false);
        }

        durations.sort_unstable();
        let p95 = durations[94];
        eprintln!("http_capture_100_seq_p95={p95:?}");
        assert!(
            p95 < Duration::from_millis(8),
            "HTTP capture p95 {p95:?} exceeded M1 target"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_requires_configured_bearer_token() {
        let path = temp_db_path("http-auth");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.bearer_token = Some("secret".to_string());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"WAL timeout fixed"}
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 401);
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 0);
        assert_eq!(table_rows(&stats, "jobs"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_auth_rejection_records_safe_audit_row() {
        let path = temp_db_path("http-auth-audit");
        let mut cfg = Config::with_db_path(path.clone());
        let configured_secret = "configuredsupersecretvalue";
        let presented_secret = "presentedsupersecretvalue";
        cfg.bearer_token = Some(configured_secret.to_string());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: format!("/v1/capture?token={presented_secret}"),
                headers: vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    (
                        "authorization".to_string(),
                        format!("Bearer {presented_secret}"),
                    ),
                ],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"WAL timeout fixed"}
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 401);
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 0);
        assert_eq!(table_rows(&stats, "jobs"), 0);
        assert_eq!(table_rows(&stats, "audit_log"), 1);
        assert_db_files_do_not_contain(&path, configured_secret);
        assert_db_files_do_not_contain(&path, presented_secret);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_accepts_configured_bearer_token() {
        let path = temp_db_path("http-auth-ok");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.bearer_token = Some("secret".to_string());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("authorization".to_string(), "Bearer secret".to_string()),
                ],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"WAL timeout fixed"}
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 202);
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 1);
        assert_eq!(table_rows(&stats, "jobs"), 1);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_rejects_invalid_json_without_writes() {
        let path = temp_db_path("http-invalid-json");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: b"{".to_vec(),
            },
        );

        assert_eq!(response.status, 400);
        assert_eq!(response.body["error"]["code"], "invalid_json");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 0);
        assert_eq!(table_rows(&stats, "jobs"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_rejects_missing_payload_without_writes() {
        let path = temp_db_path("http-missing-payload");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation"
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 422);
        assert_eq!(response.body["error"]["code"], "invalid_request");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 0);
        assert_eq!(table_rows(&stats, "jobs"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_chunked_transfer_encoding_returns_501() {
        let path = temp_db_path("http-chunked");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![
                    ("content-type".to_string(), "application/json".to_string()),
                    ("transfer-encoding".to_string(), "Chunked".to_string()),
                ],
                body: Vec::new(),
            },
        );

        assert_eq!(response.status, 501);
        assert_eq!(response.body["error"]["code"], "not_implemented");

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_rejects_string_ts_ms() {
        let path = temp_db_path("http-string-ts");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{
                    "session_id":"session-1",
                    "agent":"claude",
                    "source":"tool_result",
                    "kind":"observation",
                    "payload":{"text":"x"},
                    "ts_ms":"1234"
                }"#
                .to_vec(),
            },
        );

        assert_eq!(response.status, 422);
        assert_eq!(
            response.body["error"]["message"],
            "ts_ms must be integer milliseconds"
        );
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "raw_events"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn dream_cli_rejects_max_seconds_over_one_day() {
        let result = Cli::parse(["memoryd", "dream", "--max-seconds", "86401"].map(OsString::from));
        assert!(matches!(
            result,
            Err(CliError::Config(
                memoryd_core::config::ConfigError::CapDurationTooLarge { .. }
            ))
        ));
    }

    #[test]
    fn http_recall_returns_lexical_matches_without_provider_usage() {
        let path = temp_db_path("http-recall");
        let cfg = Config::with_db_path(path.clone());
        let mut store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);
        store
            .capture_event(NewRawEvent {
                session_id: "session-1".to_string(),
                agent: "claude".to_string(),
                source: "tool_result".to_string(),
                kind: "observation".to_string(),
                payload: serde_json::json!({"text": "WAL timeout fixed"}),
                provenance: serde_json::json!({}),
                ts_ms: 1234,
            })
            .expect("capture succeeds");

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/recall".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{"query":"wal timeout","k":5}"#.to_vec(),
            },
        );

        assert_eq!(response.status, 200);
        assert_eq!(response.body["mode"], "lexical");
        assert_eq!(response.body["results"][0]["raw_event_id"], 1);
        assert_eq!(response.body["results"][0]["content"], "WAL timeout fixed");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(table_rows(&stats, "provider_usage"), 0);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_capture_redacts_secret_payload_before_recall_returns_it() {
        let path = temp_db_path("http-redacts-before-recall");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);
        let bearer = "leakycredentialvalue";
        let api_key = "structuredapikeyvalue";
        let email = "ops@example.test";
        let body = format!(
            r#"{{
                "session_id":"session-1",
                "agent":"claude",
                "source":"tool_result",
                "kind":"observation",
                "payload":{{
                    "text":"HTTP Authorization: Bearer {bearer}; contact {email}",
                    "api_key":"{api_key}"
                }}
            }}"#
        );

        let capture_response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/capture".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: body.into_bytes(),
            },
        );
        assert_eq!(capture_response.status, 202);

        let recall_response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/recall".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{"query":"redacted","k":5}"#.to_vec(),
            },
        );

        assert_eq!(recall_response.status, 200);
        let content = recall_response.body["results"][0]["content"]
            .as_str()
            .expect("content is string");
        assert_eq!(
            content,
            "HTTP Authorization: Bearer [REDACTED]; contact [REDACTED]"
        );
        assert!(!content.contains(bearer));
        assert!(!content.contains(api_key));
        assert!(!content.contains(email));

        let secret_response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/recall".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{"query":"leakycredentialvalue","k":5}"#.to_vec(),
            },
        );
        assert_eq!(secret_response.status, 200);
        assert_eq!(
            secret_response.body["results"]
                .as_array()
                .expect("results is array")
                .len(),
            0
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn http_recall_rejects_empty_query() {
        let path = temp_db_path("http-recall-empty");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/recall".to_string(),
                headers: vec![("content-type".to_string(), "application/json".to_string())],
                body: br#"{"query":"?!","k":5}"#.to_vec(),
            },
        );

        assert_eq!(response.status, 422);
        assert_eq!(response.body["error"]["code"], "invalid_request");

        cleanup_db_files(&path);
    }

    #[test]
    fn rejects_unknown_command() {
        let err = Cli::parse(["memoryd", "nonesuch"].map(OsString::from)).expect_err("parse fails");

        assert!(matches!(err, CliError::UnknownCommand(command) if command == "nonesuch"));
    }

    fn table_rows(stats: &[TableStats], table: &str) -> i64 {
        stats
            .iter()
            .find(|stat| stat.table == table)
            .map(|stat| stat.rows)
            .unwrap_or_default()
    }

    fn temp_db_path(name: &str) -> PathBuf {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after unix epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("memoryd-{name}-{}-{nanos}.db", std::process::id()))
    }

    fn test_ip(last: u8) -> IpAddr {
        IpAddr::from([10, 0, 0, last])
    }

    /// Writer actor for handler tests. The join handle is dropped; the actor
    /// thread exits when the returned handle goes out of scope.
    fn test_writer(path: &Path) -> WriterHandle {
        let (writer, _thread) =
            memoryd_core::writer::Writer::spawn(path).expect("writer spawns for test");
        writer
    }

    #[test]
    fn auth_throttle_allows_under_limit() {
        let throttle = AuthThrottle::new();
        let ip = test_ip(1);
        for _ in 0..(AUTH_FAIL_LIMIT - 1) {
            throttle.record_failure(ip, 0);
        }
        assert!(!throttle.is_throttled(ip, 1_000));
    }

    #[test]
    fn auth_throttle_blocks_after_limit_failures_in_window() {
        let throttle = AuthThrottle::new();
        let ip = test_ip(2);
        for _ in 0..AUTH_FAIL_LIMIT {
            throttle.record_failure(ip, 0);
        }
        assert!(throttle.is_throttled(ip, 1_000));
    }

    #[test]
    fn auth_throttle_unblocks_after_lockout_expires() {
        let throttle = AuthThrottle::new();
        let ip = test_ip(3);
        for _ in 0..AUTH_FAIL_LIMIT {
            throttle.record_failure(ip, 0);
        }
        let after_lockout = AUTH_LOCKOUT_MS + 1_000;
        assert!(!throttle.is_throttled(ip, after_lockout));
        // The failure count was reset at lockout; one new failure must not re-lock.
        throttle.record_failure(ip, after_lockout);
        assert!(!throttle.is_throttled(ip, after_lockout + 1));
    }

    #[test]
    fn auth_throttle_success_clears_failures() {
        let throttle = AuthThrottle::new();
        let ip = test_ip(4);
        for _ in 0..(AUTH_FAIL_LIMIT - 1) {
            throttle.record_failure(ip, 0);
        }
        throttle.record_success(ip);
        for _ in 0..(AUTH_FAIL_LIMIT - 1) {
            throttle.record_failure(ip, 1);
        }
        assert!(!throttle.is_throttled(ip, 2));
    }

    #[test]
    fn auth_throttle_evicts_oldest_when_full() {
        let throttle = AuthThrottle::new();
        // Lock out every entry so eviction cannot reclaim them as expired.
        for index in 0..AUTH_THROTTLE_MAX_ENTRIES {
            let ip = IpAddr::from([
                10,
                1,
                u8::try_from(index / 256).expect("index fits"),
                u8::try_from(index % 256).expect("index fits"),
            ]);
            for _ in 0..AUTH_FAIL_LIMIT {
                throttle.record_failure(ip, index as i64);
            }
        }
        let newcomer = IpAddr::from([10, 2, 0, 1]);
        throttle.record_failure(newcomer, 10);
        let map = throttle.inner.lock().expect("throttle lock");
        assert!(map.len() <= AUTH_THROTTLE_MAX_ENTRIES);
        assert!(map.contains_key(&newcomer), "newcomer was admitted");
        assert!(
            !map.contains_key(&IpAddr::from([10, 1, 0, 0])),
            "oldest entry was evicted"
        );
    }

    /// Spawn `serve_loop` on an ephemeral loopback port with short timeouts so
    /// socket-level behavior (slowloris, throttling, shutdown) is testable end
    /// to end. Returns the bound address, the loop's join handle, and the
    /// shutdown flag.
    fn spawn_test_server(
        cfg: Config,
    ) -> (
        SocketAddr,
        std::thread::JoinHandle<Result<(), CliError>>,
        Arc<AtomicBool>,
    ) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("test listener binds");
        let addr = listener.local_addr().expect("listener has local addr");
        let cfg = Arc::new(cfg);
        let throttle = Arc::new(AuthThrottle::new());
        let shutdown = Arc::new(AtomicBool::new(false));
        let loop_shutdown = Arc::clone(&shutdown);
        let (writer, _writer_thread) =
            memoryd_core::writer::Writer::spawn(&cfg.db_path).expect("writer spawns for test");
        let handle = std::thread::spawn(move || {
            serve_loop(
                listener,
                cfg,
                throttle,
                writer,
                loop_shutdown,
                Duration::from_millis(200),
                Duration::from_secs(5),
            )
        });
        (addr, handle, shutdown)
    }

    fn send_raw_request(addr: SocketAddr, raw: &str) -> String {
        let mut stream = TcpStream::connect(addr).expect("test client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        stream
            .write_all(raw.as_bytes())
            .expect("test client writes");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }

    fn capture_request_raw(token: &str) -> String {
        let body = r#"{"session_id":"s1","agent":"a","source":"hook","kind":"observation","payload":{"text":"tcp test"}}"#;
        format!(
            "POST /v1/capture HTTP/1.1\r\nHost: localhost\r\nAuthorization: Bearer {token}\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
            body.len()
        )
    }

    #[test]
    fn tcp_idle_connection_receives_408_timeout() {
        let path = temp_db_path("tcp-idle-408");
        let (addr, _handle, _shutdown) = spawn_test_server(Config::with_db_path(path.clone()));

        let mut stream = TcpStream::connect(addr).expect("test client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        // Send nothing: the server's read deadline must produce a 408.
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 408"),
            "idle connection should time out with 408, got: {response}"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn tcp_slow_client_does_not_block_concurrent_request() {
        let path = temp_db_path("tcp-slowloris");
        let (addr, _handle, _shutdown) = spawn_test_server(Config::with_db_path(path.clone()));

        // Hold an idle connection open, then issue a real request on a second
        // connection; it must complete promptly despite the stalled peer.
        let _idle = TcpStream::connect(addr).expect("idle connection connects");
        let started = Instant::now();
        let response = send_raw_request(addr, &capture_request_raw("unused"));
        assert!(
            response.starts_with("HTTP/1.1 202"),
            "concurrent capture should succeed, got: {response}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "concurrent request must not wait behind the idle connection"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn tcp_repeated_auth_failures_return_429() {
        let path = temp_db_path("tcp-auth-throttle");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.bearer_token = Some("correct-horse-battery-staple".to_string());
        let (addr, _handle, _shutdown) = spawn_test_server(cfg);

        for attempt in 0..AUTH_FAIL_LIMIT {
            let response = send_raw_request(addr, &capture_request_raw("wrong-token"));
            assert!(
                response.starts_with("HTTP/1.1 401"),
                "attempt {attempt} should be 401, got: {response}"
            );
        }
        let response = send_raw_request(addr, &capture_request_raw("wrong-token"));
        assert!(
            response.starts_with("HTTP/1.1 429"),
            "post-limit attempt should be throttled, got: {response}"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn tcp_concurrent_captures_through_writer_all_persist() {
        let path = temp_db_path("tcp-writer-concurrent");
        let (addr, _handle, _shutdown) = spawn_test_server(Config::with_db_path(path.clone()));

        let threads: Vec<_> = (0..8)
            .map(|index| {
                std::thread::spawn(move || {
                    let body = format!(
                        r#"{{"session_id":"s{index}","agent":"a","source":"hook","kind":"observation","payload":{{"text":"concurrent {index}"}}}}"#
                    );
                    let raw = format!(
                        "POST /v1/capture HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{body}",
                        body.len()
                    );
                    send_raw_request(addr, &raw)
                })
            })
            .collect();
        for thread in threads {
            let response = thread.join().expect("capture thread joins");
            assert!(
                response.starts_with("HTTP/1.1 202"),
                "every concurrent capture is accepted, got: {response}"
            );
        }

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("table stats");
        assert_eq!(
            table_rows(&stats, "raw_events"),
            8,
            "all writes serialized through the writer actor persisted"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn serve_loop_exits_when_shutdown_flag_set() {
        let path = temp_db_path("serve-shutdown");
        let (_addr, handle, shutdown) = spawn_test_server(Config::with_db_path(path.clone()));

        shutdown.store(true, Ordering::Release);
        let started = Instant::now();
        let result = handle.join().expect("serve loop thread joins");
        assert!(result.is_ok(), "serve loop exits cleanly on shutdown");
        assert!(
            started.elapsed() < Duration::from_secs(5),
            "shutdown must be observed promptly"
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn serve_loop_finishes_inflight_request_during_shutdown() {
        let path = temp_db_path("serve-shutdown-inflight");
        let (addr, handle, shutdown) = spawn_test_server(Config::with_db_path(path.clone()));

        // Open the connection first, signal shutdown, then send the request:
        // the already-accepted (or about-to-be-accepted) work must still get a
        // response while the loop drains.
        let mut stream = TcpStream::connect(addr).expect("test client connects");
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .expect("client read timeout");
        let raw = capture_request_raw("unused");
        stream
            .write_all(raw.as_bytes())
            .expect("test client writes");
        std::thread::sleep(Duration::from_millis(100));
        shutdown.store(true, Ordering::Release);
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        assert!(
            response.starts_with("HTTP/1.1 202"),
            "in-flight request still answered, got: {response}"
        );
        assert!(handle.join().expect("loop joins").is_ok());

        cleanup_db_files(&path);
    }

    #[test]
    fn http_health_get_on_loopback_bypasses_bearer_auth() {
        let path = temp_db_path("http-health-loopback");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.bearer_token = Some("configured-token-0123456789".to_string());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "GET".to_string(),
                path: "/v1/health".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            },
        );

        assert_eq!(response.status, 200);
        assert_eq!(response.body["status"], "ok");
        assert!(
            response.body["schema_version"].is_i64(),
            "schema_version is an integer: {}",
            response.body
        );

        cleanup_db_files(&path);
    }

    #[test]
    fn http_health_requires_bearer_for_non_loopback() {
        let path = temp_db_path("http-health-remote");
        let mut cfg = Config::with_db_path(path.clone());
        cfg.bind = "0.0.0.0:7077".parse().expect("bind parses");
        cfg.bearer_token = Some("configured-token-0123456789".to_string());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("203.0.113.9:50000".parse().expect("peer parses")),
            HttpRequest {
                method: "GET".to_string(),
                path: "/v1/health".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        assert_eq!(response.status, 401);

        let authorized = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("203.0.113.9:50000".parse().expect("peer parses")),
            HttpRequest {
                method: "GET".to_string(),
                path: "/v1/health".to_string(),
                headers: vec![(
                    "authorization".to_string(),
                    "Bearer configured-token-0123456789".to_string(),
                )],
                body: Vec::new(),
            },
        );
        assert_eq!(authorized.status, 200);

        cleanup_db_files(&path);
    }

    #[test]
    fn http_health_rejects_post() {
        let path = temp_db_path("http-health-post");
        let cfg = Config::with_db_path(path.clone());
        let store = Store::open(&path).expect("store opens");
        let writer = test_writer(&path);

        let response = handle_http_request(
            &store,
            &writer,
            &cfg,
            Some("127.0.0.1:65000".parse().expect("peer parses")),
            HttpRequest {
                method: "POST".to_string(),
                path: "/v1/health".to_string(),
                headers: Vec::new(),
                body: Vec::new(),
            },
        );
        assert_eq!(response.status, 405);

        cleanup_db_files(&path);
    }

    fn cleanup_db_files(path: &Path) {
        for suffix in ["", "-shm", "-wal"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            let _ = fs::remove_file(file);
        }
    }

    fn assert_db_files_do_not_contain(path: &Path, needle: &str) {
        let needle = needle.as_bytes();
        for suffix in ["", "-shm", "-wal"] {
            let file = PathBuf::from(format!("{}{suffix}", path.display()));
            let Ok(bytes) = fs::read(file) else {
                continue;
            };
            assert!(
                !bytes.windows(needle.len()).any(|window| window == needle),
                "database files leaked secret bytes"
            );
        }
    }

    #[test]
    fn parses_recall_with_hops() {
        let cli = Cli::parse(["memoryd", "recall", "q", "--hops", "0"].map(OsString::from))
            .expect("cli parses");
        let Command::Recall(args) = cli.command else {
            panic!("expected recall command");
        };
        assert_eq!(args.hops, 0, "--hops 0 disables expansion");

        let default = Cli::parse(["memoryd", "recall", "q"].map(OsString::from)).expect("parses");
        let Command::Recall(args) = default.command else {
            panic!("expected recall command");
        };
        assert_eq!(args.hops, 1, "one-hop expansion is the default");

        assert!(
            Cli::parse(["memoryd", "recall", "q", "--hops", "2"].map(OsString::from)).is_err(),
            "--hops only accepts 0 or 1"
        );
    }

    #[test]
    fn recall_command_one_hop_returns_linked_memory() {
        let path = temp_db_path("recall-cli-onehop");
        {
            let mut store = Store::open(&path).expect("store opens");
            for (i, text) in ["wal busy timeout fix", "vacuum schedule weekly"]
                .iter()
                .enumerate()
            {
                store
                    .capture_event(NewRawEvent {
                        session_id: "s1".to_string(),
                        agent: "claude".to_string(),
                        source: "tool_result".to_string(),
                        kind: "observation".to_string(),
                        payload: serde_json::json!({ "text": text }),
                        provenance: serde_json::json!({}),
                        ts_ms: 1000 + i as i64,
                    })
                    .expect("capture succeeds");
            }
        }
        // Consolidate + associate via the dream CLI handler.
        let cli = Cli::parse(
            ["memoryd", "dream", "--now", "--db", path.to_str().unwrap()].map(OsString::from),
        )
        .expect("cli parses");
        let Command::Dream(dargs) = cli.command.clone() else {
            panic!("expected dream");
        };
        dream(cli, dargs).expect("dream succeeds");

        let store = Store::open(&path).expect("store opens");
        let args = RecallArgs {
            query: "wal".to_string(),
            limit: 5,
            semantic: false,
            hops: 1,
            index_kind: None,
        };
        let RecallOutput::Memory(result) = recall_with_mode(
            &store,
            &args,
            "brute-force",
            &memoryd_core::adapters::AdapterKind::from_default_adapter("null"),
        )
        .expect("recall") else {
            panic!("memory corpus exists, so recall should return memories");
        };
        assert_eq!(result.mode, "memory+graph");
        let has_vacuum = result
            .hits
            .iter()
            .any(|h| h.content.contains("vacuum") && h.via_hop);
        assert!(
            has_vacuum,
            "the linked 'vacuum' memory surfaces via one hop"
        );

        // The JSON envelope is memory-shaped.
        let json = recall_response_json(&RecallOutput::Memory(result)).expect("json");
        assert!(json.contains("\"memory_id\""));
        assert!(json.contains("\"via_hop\""));

        // hops=0 does not surface the unrelated neighbor.
        let args0 = RecallArgs {
            query: "wal".to_string(),
            limit: 5,
            semantic: false,
            hops: 0,
            index_kind: None,
        };
        let RecallOutput::Memory(direct) = recall_with_mode(
            &store,
            &args0,
            "brute-force",
            &memoryd_core::adapters::AdapterKind::from_default_adapter("null"),
        )
        .expect("recall") else {
            panic!("expected memories");
        };
        assert!(
            !direct.hits.iter().any(|h| h.content.contains("vacuum")),
            "hops=0 returns only the direct lexical match"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn recall_request_from_json_parses_hops() {
        let args = recall_request_from_json(serde_json::json!({"query": "wal", "hops": 0}))
            .expect("parses");
        assert_eq!(args.hops, 0);
        let default =
            recall_request_from_json(serde_json::json!({"query": "wal"})).expect("parses");
        assert_eq!(default.hops, 1, "hops defaults to 1");
        assert!(
            recall_request_from_json(serde_json::json!({"query": "wal", "hops": 5})).is_err(),
            "hops must be 0 or 1"
        );
    }

    #[test]
    fn parses_approve_flags() {
        let cli = Cli::parse(["memoryd", "approve", "--list"].map(OsString::from)).expect("parses");
        let Command::Approve(args) = cli.command else {
            panic!("expected approve")
        };
        assert!(args.list && args.id.is_none() && !args.accept && !args.reject);

        let cli =
            Cli::parse(["memoryd", "approve", "--id", "abc123", "--accept"].map(OsString::from))
                .expect("parses");
        let Command::Approve(args) = cli.command else {
            panic!("expected approve")
        };
        assert_eq!(args.id.as_deref(), Some("abc123"));
        assert!(args.accept && !args.reject);
    }

    #[test]
    fn approve_rejects_accept_and_reject_together() {
        let path = temp_db_path("m8-cli-conflict");
        let cli = Cli::parse(
            [
                "memoryd",
                "approve",
                "--id",
                "x",
                "--accept",
                "--reject",
                "--db",
                path.to_str().unwrap(),
            ]
            .map(OsString::from),
        )
        .expect("parses");
        let Command::Approve(args) = cli.command.clone() else {
            panic!("expected approve")
        };
        assert!(
            approve(cli, args).is_err(),
            "--accept and --reject are mutually exclusive"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn approve_command_end_to_end_commits_an_approved_fact() {
        let path = temp_db_path("m8-cli-e2e");
        {
            let mut store = Store::open(&path).expect("store opens");
            store
                .capture_event(NewRawEvent {
                    session_id: "s1".to_string(),
                    agent: "claude".to_string(),
                    source: "tool_result".to_string(),
                    kind: "preference".to_string(),
                    payload: serde_json::json!({"text": "prefers conventional commits"}),
                    provenance: serde_json::json!({}),
                    ts_ms: 1000,
                })
                .expect("capture");
        }
        // dream: consolidate -> ... -> extract proposes a pending approval.
        let cli = Cli::parse(
            ["memoryd", "dream", "--now", "--db", path.to_str().unwrap()].map(OsString::from),
        )
        .expect("parses");
        let Command::Dream(dargs) = cli.command.clone() else {
            panic!("expected dream")
        };
        dream(cli, dargs).expect("dream");

        let id = {
            let store = Store::open(&path).expect("store opens");
            let pending = store.list_pending_approvals(10).expect("list");
            assert_eq!(pending.len(), 1, "one pending profile-fact approval");
            pending[0].id.clone()
        };

        // approve --id <id> --accept commits the fact.
        let cli = Cli::parse(
            [
                "memoryd",
                "approve",
                "--id",
                &id,
                "--accept",
                "--db",
                path.to_str().unwrap(),
            ]
            .map(OsString::from),
        )
        .expect("parses");
        let Command::Approve(aargs) = cli.command.clone() else {
            panic!("expected approve")
        };
        approve(cli, aargs).expect("approve");

        let store = Store::open(&path).expect("store opens");
        let stats = store.table_stats().expect("stats");
        assert_eq!(
            table_rows(&stats, "profile_facts"),
            1,
            "fact committed after approval"
        );
        assert!(
            store.list_pending_approvals(10).expect("list").is_empty(),
            "the approval is no longer pending after the decision"
        );
        cleanup_db_files(&path);
    }

    #[test]
    fn parses_recall_with_index_flag() {
        let cli = Cli::parse(
            ["memoryd", "recall", "q", "--semantic", "--index", "hnsw"].map(OsString::from),
        )
        .expect("parses");
        let Command::Recall(args) = cli.command else {
            panic!("expected recall")
        };
        assert_eq!(args.index_kind.as_deref(), Some("hnsw"));
    }

    #[test]
    fn recall_rejects_unknown_index_kind() {
        let path = temp_db_path("recall-bad-index");
        let cli = Cli::parse(
            [
                "memoryd",
                "recall",
                "q",
                "--index",
                "bogus",
                "--db",
                path.to_str().unwrap(),
            ]
            .map(OsString::from),
        )
        .expect("parses");
        let Command::Recall(args) = cli.command.clone() else {
            panic!("expected recall")
        };
        assert!(
            recall(cli, args).is_err(),
            "unknown --index value is rejected"
        );
        cleanup_db_files(&path);
    }
}
