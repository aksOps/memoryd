#![forbid(unsafe_code)]

use memoryd_core::config::{Config, DEFAULT_BIND};
use memoryd_core::store::{CaptureAck, NewRawEvent, RecallResult, Store, StoreError};
use std::env;
use std::ffi::OsString;
use std::fmt;
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{SystemTime, UNIX_EPOCH};

const MAX_HTTP_LINE_BYTES: usize = 8 * 1024;
const MAX_HTTP_HEADERS: usize = 64;
const MAX_HTTP_BODY_BYTES: usize = 64 * 1024;

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
    let result = recall_with_mode(&store, &args)?;
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
        return Err(CliError::Store(StoreError::Import(format!(
            "unsupported import source {:?}; only \"jsonl\" is supported in this build",
            args.source
        ))));
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

/// Run semantic recall when requested, else lexical. Only the no-spend `null`
/// adapter ships today, and it self-degrades to lexical (`embeds_semantically`
/// is false), so `--semantic` is safe by default; a configured non-`null` embedding
/// provider (deferred M3 increment) activates real rerank with no caller change.
fn recall_with_mode(store: &Store, args: &RecallArgs) -> Result<RecallResult, StoreError> {
    if args.semantic {
        let adapter = memoryd_core::adapters::NullAdapter::new();
        let index = memoryd_core::vectorindex::BruteForce;
        store.recall_semantic(&args.query, args.limit, &adapter, &index, unix_ms_now())
    } else {
        store.recall_events(&args.query, args.limit)
    }
}

fn serve(cli: Cli) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let mut store = Store::open(&cfg.db_path)?;

    let worker_db = cfg.db_path.clone();
    let worker_caps = cfg.caps.clone();
    // Detached for M3: the worker runs for the process lifetime. Graceful shutdown
    // (draining in-flight jobs) and consolidating onto the planned single-writer
    // actor (ARCHITECTURE-PLAN s7.1/U5) are deferred; today it is a second writer.
    let _worker = std::thread::spawn(move || {
        let adapter = memoryd_core::adapters::NullAdapter::new();
        let mut worker_store = match Store::open(&worker_db) {
            Ok(store) => store,
            Err(err) => {
                eprintln!("memoryd: worker store open failed: {err}");
                return;
            }
        };
        loop {
            let now = unix_ms_now();
            match memoryd_core::worker::tick_embed(&mut worker_store, &adapter, &worker_caps, now) {
                Ok(report) if report.leased == 0 => {
                    std::thread::sleep(std::time::Duration::from_millis(250));
                }
                Ok(_) => {}
                Err(err) => {
                    eprintln!("memoryd: worker tick failed: {err}");
                    std::thread::sleep(std::time::Duration::from_millis(500));
                }
            }
        }
    });

    let listener = TcpListener::bind(cfg.bind)?;
    println!("memoryd serve");
    println!("bind: {}", cfg.bind);
    println!("db_path: {}", cfg.db_path.display());
    println!("worker: embed (null adapter)");

    for stream in listener.incoming() {
        match stream {
            Ok(stream) => {
                if let Err(err) = handle_http_connection(&mut store, &cfg, stream) {
                    eprintln!("memoryd: request failed: {err}");
                }
            }
            Err(err) => return Err(CliError::Io(err)),
        }
    }

    Ok(())
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Command {
    Doctor,
    Stats,
    Serve,
    Remember(RememberArgs),
    Recall(RecallArgs),
    Import(ImportArgs),
    Help,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RememberArgs {
    content: String,
    kind: String,
    session_id: String,
    source: String,
    tags: Vec<String>,
}

impl Default for RememberArgs {
    fn default() -> Self {
        Self {
            content: String::new(),
            kind: "note".to_string(),
            session_id: "cli".to_string(),
            source: "cli".to_string(),
            tags: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct RecallArgs {
    query: String,
    limit: usize,
    semantic: bool,
}

impl Default for RecallArgs {
    fn default() -> Self {
        Self {
            query: String::new(),
            limit: 5,
            semantic: false,
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

#[derive(Debug, Clone)]
struct Cli {
    command: Command,
    db_path: PathBuf,
    bind: SocketAddr,
    bearer_token: Option<String>,
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
        })
    }

    fn config(&self) -> Result<Config, CliError> {
        let mut cfg = Config::with_db_path(self.db_path.clone());
        cfg.bind = self.bind;
        cfg.bearer_token = self.bearer_token.clone();
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
            memoryd doctor [--db <path>] [--bind <addr:port>] [--token <token>]\n\
            memoryd stats  [--db <path>] [--bind <addr:port>] [--token <token>]\n\
            memoryd remember <content> [--kind <kind>] [--session <id>] [--source <source>] [--tags <a,b>] [--db <path>]\n\
            memoryd recall <query> [--k <limit>] [--semantic] [--db <path>]\n\
            memoryd import --source jsonl --path <file> [--db <path>]\n\
            memoryd serve [--db <path>] [--bind <addr:port>] [--token <token>]\n\n\
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
        agent: "cli".to_string(),
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

fn recall_response_json(result: &RecallResult) -> Result<String, CliError> {
    Ok(serde_json::to_string(&recall_response_value(result))?)
}

fn recall_response_value(result: &RecallResult) -> serde_json::Value {
    let hits = result
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
        "degraded": result.degraded,
        "mode": result.mode,
        "compared": result.compared,
    })
}

fn handle_http_connection(
    store: &mut Store,
    cfg: &Config,
    mut stream: TcpStream,
) -> Result<(), CliError> {
    let peer = stream.peer_addr().ok();
    let response = match read_http_request(&mut stream) {
        Ok(request) => handle_http_request(store, cfg, peer, request),
        Err(err) => HttpResponse::error(err.status, err.code, err.message),
    };
    write_http_response(&mut stream, response)?;
    Ok(())
}

fn handle_http_request(
    store: &mut Store,
    cfg: &Config,
    peer: Option<SocketAddr>,
    request: HttpRequest,
) -> HttpResponse {
    if !is_authorized(cfg, peer, &request.headers) {
        let peer_loopback = peer.map(|addr| addr.ip().is_loopback());
        let authorization_header_present =
            header_value(&request.headers, "authorization").is_some();
        if store
            .record_auth_rejection(
                &request.method,
                &request.path,
                peer_loopback,
                authorization_header_present,
                auth_rejection_reason(cfg, peer),
            )
            .is_err()
        {
            return HttpResponse::error(500, "store_error", "auth audit could not be persisted");
        }
        return HttpResponse::error(401, "unauthorized", "authorization failed");
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
        "/v1/capture" => handle_http_capture(store, body, cfg.caps.queue_depth_max),
        "/v1/recall" => handle_http_recall(store, body),
        _ => HttpResponse::error(404, "not_found", "route not found"),
    }
}

fn handle_http_capture(
    store: &mut Store,
    body: serde_json::Value,
    max_active_jobs: usize,
) -> HttpResponse {
    let event = match capture_event_from_json(body) {
        Ok(event) => event,
        Err(message) => return HttpResponse::error(422, "invalid_request", message),
    };

    match store.capture_event_with_queue_limit(event, max_active_jobs) {
        Ok(ack) => HttpResponse::json(
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
        Err(StoreError::InvalidCaptureField(_)) => {
            HttpResponse::error(422, "invalid_request", "capture fields must not be empty")
        }
        Err(_) => HttpResponse::error(500, "store_error", "capture could not be persisted"),
    }
}

fn handle_http_recall(store: &Store, body: serde_json::Value) -> HttpResponse {
    let args = match recall_request_from_json(body) {
        Ok(args) => args,
        Err(message) => return HttpResponse::error(422, "invalid_request", message),
    };

    match recall_with_mode(store, &args) {
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
            stream.read_exact(&mut body).map_err(|_| {
                HttpParseError::new(400, "bad_request", "could not read request body")
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
        Some(value) if value.is_string() => unix_ms_now(),
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

    Ok(RecallArgs {
        query,
        limit,
        semantic,
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
            413 => "Payload Too Large",
            415 => "Unsupported Media Type",
            422 => "Unprocessable Entity",
            431 => "Request Header Fields Too Large",
            500 => "Internal Server Error",
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
        };
        let result = recall_with_mode(&store, &args).expect("recall succeeds");

        // The only shipped adapter is null, which self-degrades
        // (embeds_semantically=false), so `--semantic` returns lexical-shaped results
        // flagged degraded — no provider spend, no query embedding cached.
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
    fn http_capture_route_persists_event_on_loopback() {
        let path = temp_db_path("http-capture");
        let cfg = Config::with_db_path(path.clone());
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");
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
                &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
    fn http_recall_returns_lexical_matches_without_provider_usage() {
        let path = temp_db_path("http-recall");
        let cfg = Config::with_db_path(path.clone());
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

        let response = handle_http_request(
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");
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
            &mut store,
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
            &mut store,
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
            &mut store,
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
        let mut store = Store::open(&path).expect("store opens");

        let response = handle_http_request(
            &mut store,
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
}
