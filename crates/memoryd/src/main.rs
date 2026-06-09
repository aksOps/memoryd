#![forbid(unsafe_code)]

use memoryd_core::config::{Config, DEFAULT_BIND};
use memoryd_core::store::{CaptureAck, NewRawEvent, Store, StoreError};
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
    let ack = store.capture_event(remember_event(args))?;
    println!("{}", remember_response_json(&ack)?);
    Ok(())
}

fn serve(cli: Cli) -> Result<(), CliError> {
    let cfg = cli.config()?;
    cfg.validate()?;

    let mut store = Store::open(&cfg.db_path)?;
    let listener = TcpListener::bind(cfg.bind)?;
    println!("memoryd serve");
    println!("bind: {}", cfg.bind);
    println!("db_path: {}", cfg.db_path.display());

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
                "--source" => {
                    let Command::Remember(remember) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    remember.source = next_string(&mut args, "--source")?;
                }
                "--tags" => {
                    let Command::Remember(remember) = &mut command else {
                        return Err(CliError::UnknownFlag(token));
                    };
                    remember.tags = parse_tags(&next_string(&mut args, "--tags")?);
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
                other if matches!(command, Command::Remember(_)) && !other.starts_with("--") => {
                    let content = raw
                        .into_string()
                        .map_err(|_| CliError::InvalidUtf8Argument("content"))?;
                    let Command::Remember(remember) = &mut command else {
                        unreachable!("remember command checked above");
                    };
                    if !remember.content.is_empty() {
                        return Err(CliError::UnexpectedArgument(content));
                    }
                    remember.content = content;
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

fn remember_response_json(ack: &CaptureAck) -> Result<String, CliError> {
    Ok(serde_json::to_string(&serde_json::json!({
        "raw_event_id": ack.raw_event_id,
        "session_id": ack.session_id,
        "enqueued_job_id": ack.enqueued_job_id,
        "pending_memory": true,
        "degraded": ack.degraded,
    }))?)
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
        return HttpResponse::error(401, "unauthorized", "authorization failed");
    }

    if request.path != "/v1/capture" {
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
    let event = match capture_event_from_json(body) {
        Ok(event) => event,
        Err(message) => return HttpResponse::error(422, "invalid_request", message),
    };

    match store.capture_event(event) {
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
}
