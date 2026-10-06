//! `ovm limits serve` — `limits.json` over HTTP, in the shape a CLIProxyAPI
//! management hub answers, so a client that reads usage from such a hub
//! (t3code's `usageLimitSources`) can read ovm's numbers instead.
//!
//! Read-only and credential-free: it answers the two calls such a client
//! makes, `GET /v0/management/auth-files` and `POST /v0/management/api-call`,
//! from the merged snapshot on disk. The `url` an api-call names is matched,
//! never fetched — nothing here talks to a provider, and no token is held, so
//! the `$TOKEN$` placeholder a client sends stays a placeholder. Anything that
//! would change an account (a reset credit, `reset-quota`) is refused.
//!
//! Listens on the tailnet or on loopback only, and every call carries the
//! bearer key kept in `serve.key` beside `limits.json`.

use crate::paths::LimitsDirs;
use crate::registry::Provider;
use crate::snapshot::{self, AccountSnapshot, Merged, Window};
use crate::{LimitsError, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{IpAddr, SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// CLIProxyAPI's own default port, so a client's hub URL only changes host.
pub const DEFAULT_PORT: u16 = 8317;

pub const CLAUDE_USAGE_URL: &str = "https://api.anthropic.com/api/oauth/usage";
pub const CODEX_USAGE_URL: &str = "https://chatgpt.com/backend-api/wham/usage";

const KEY_FILE: &str = "serve.key";
const KEY_BYTES: usize = 32;
const MAX_HEADER_BYTES: usize = 16 * 1024;
const MAX_BODY_BYTES: usize = 64 * 1024;
const IO_TIMEOUT: Duration = Duration::from_secs(10);
/// The whole request must arrive within this, however slowly it trickles.
const REQUEST_DEADLINE: Duration = Duration::from_secs(15);
/// Connections served at once; more are closed on arrival. A hub client
/// makes a handful of calls per refresh.
const MAX_CONNECTIONS: usize = 16;

/// What `api-call` answers for a URL this hub will not serve: the client
/// reports "the provider refused the hub request (HTTP 501)".
const NOT_SUPPORTED: u16 = 501;

/// Where the listener goes: the tailnet address, loopback, or one given.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Bind {
    Tailnet(u16),
    Local(u16),
    Addr(SocketAddr),
}

impl Bind {
    /// `tailnet`, `local`, either with `:PORT`, or `ADDR:PORT`.
    pub fn parse(text: &str) -> Result<Self> {
        let (host, port) = match text.rsplit_once(':') {
            Some((host, port)) if host == "tailnet" || host == "local" => {
                let port = port
                    .parse()
                    .map_err(|_| message(format!("`{port}` is not a port")))?;
                (host, port)
            }
            _ => (text, DEFAULT_PORT),
        };
        match host {
            "tailnet" => Ok(Self::Tailnet(port)),
            "local" => Ok(Self::Local(port)),
            _ => {
                let addr: SocketAddr = text.parse().map_err(|_| {
                    message(format!(
                        "`{text}` is not tailnet, local, or an ADDR:PORT (e.g. 100.64.0.1:{DEFAULT_PORT})"
                    ))
                })?;
                if !allowed_peer(addr.ip()) {
                    return Err(message(format!(
                        "{} is neither loopback nor a tailnet address — serve stays off the open network",
                        addr.ip()
                    )));
                }
                Ok(Self::Addr(addr))
            }
        }
    }

    fn resolve(&self) -> Result<SocketAddr> {
        match self {
            Self::Local(port) => Ok(SocketAddr::from(([127, 0, 0, 1], *port))),
            Self::Addr(addr) => Ok(*addr),
            Self::Tailnet(port) => Ok(SocketAddr::new(tailnet_ip()?, *port)),
        }
    }
}

/// This machine's tailnet IPv4, as `tailscale ip -4` reports it.
fn tailnet_ip() -> Result<IpAddr> {
    let output = std::process::Command::new("tailscale")
        .args(["ip", "-4"])
        .output()
        .map_err(|_| message("tailscale is not on PATH — use --bind local".into()))?;
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .find_map(|line| line.trim().parse::<IpAddr>().ok())
        .filter(|ip| is_tailnet(*ip))
        .ok_or_else(|| message("tailscale reports no tailnet address — use --bind local".into()))
}

/// Tailscale hands out 100.64.0.0/10 and fd7a:115c:a1e0::/48.
pub fn is_tailnet(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let [a, b, ..] = v4.octets();
            a == 100 && (64..128).contains(&b)
        }
        IpAddr::V6(v6) => v6.segments()[..3] == [0xfd7a, 0x115c, 0xa1e0],
    }
}

pub fn allowed_peer(ip: IpAddr) -> bool {
    ip.is_loopback() || is_tailnet(ip)
}

/// The bearer key, made on first use: 32 random bytes as hex, mode 0600.
pub fn load_or_create_key(dirs: &LimitsDirs) -> Result<String> {
    let path = dirs.base().join(KEY_FILE);
    match std::fs::read_to_string(&path) {
        Ok(key) if !key.trim().is_empty() => return Ok(key.trim().to_string()),
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let mut bytes = [0u8; KEY_BYTES];
    std::fs::File::open("/dev/urandom")?.read_exact(&mut bytes)?;
    let key: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::create_dir_all(dirs.base())?;
    write_private(&path, key.as_bytes())?;
    Ok(key)
}

fn write_private(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut file = std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes)?;
    Ok(())
}

pub fn key_path(dirs: &LimitsDirs) -> std::path::PathBuf {
    dirs.base().join(KEY_FILE)
}

/// Serve until killed.
pub fn run(dirs: &LimitsDirs, bind: &Bind) -> Result<()> {
    let key = load_or_create_key(dirs)?;
    let addr = bind.resolve()?;
    let listener = TcpListener::bind(addr)?;
    println!("  serving limits.json as a read-only hub on http://{addr}");
    println!(
        "  bearer key: {}  (print it: ovm limits serve --key)",
        crate::paths::display(&key_path(dirs))
    );
    let open = Arc::new(AtomicUsize::new(0));
    for stream in listener.incoming() {
        let Ok(stream) = stream else { continue };
        let Ok(peer) = stream.peer_addr() else {
            continue;
        };
        if !allowed_peer(peer.ip()) {
            continue;
        }
        if open.load(Ordering::SeqCst) >= MAX_CONNECTIONS {
            continue;
        }
        open.fetch_add(1, Ordering::SeqCst);
        let dirs = dirs.clone();
        let key = key.clone();
        let done = Arc::clone(&open);
        let spawned = std::thread::Builder::new().spawn(move || {
            let _ = serve_connection(stream, &dirs, &key);
            done.fetch_sub(1, Ordering::SeqCst);
        });
        if spawned.is_err() {
            open.fetch_sub(1, Ordering::SeqCst);
        }
    }
    Ok(())
}

fn serve_connection(mut stream: TcpStream, dirs: &LimitsDirs, key: &str) -> Result<()> {
    stream.set_read_timeout(Some(IO_TIMEOUT))?;
    stream.set_write_timeout(Some(IO_TIMEOUT))?;
    let (status, body) = match read_request(&stream, REQUEST_DEADLINE) {
        Ok(request) => {
            let merged = snapshot::load_merged(dirs).ok().flatten();
            handle(&request, key, merged.as_ref())
        }
        Err(_) => (400, json!({ "error": "bad request" })),
    };
    let body = body.to_string();
    let head = format!(
        "HTTP/1.1 {status} {}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        reason(status),
        body.len()
    );
    stream.write_all(head.as_bytes())?;
    stream.write_all(body.as_bytes())?;
    Ok(())
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Request {
    pub method: String,
    pub path: String,
    pub authorization: Option<String>,
    pub body: Vec<u8>,
}

/// A stream whose reads stop at a fixed instant, so a client sending a byte
/// every few seconds cannot hold a connection past the request deadline.
struct Deadline<'a> {
    stream: &'a TcpStream,
    until: Instant,
}

impl Read for Deadline<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let left = self.until.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        self.stream.set_read_timeout(Some(left.min(IO_TIMEOUT)))?;
        let mut stream = self.stream;
        stream.read(buf)
    }
}

fn read_request(stream: &TcpStream, limit: Duration) -> Result<Request> {
    let deadline = Deadline {
        stream,
        until: Instant::now() + limit,
    };
    let mut reader = BufReader::new(deadline.take((MAX_HEADER_BYTES + MAX_BODY_BYTES) as u64));
    let mut line = String::new();
    reader.read_line(&mut line)?;
    let mut parts = line.split_whitespace();
    let mut request = Request {
        method: parts.next().unwrap_or_default().to_string(),
        path: parts.next().unwrap_or_default().to_string(),
        ..Request::default()
    };
    let mut header_bytes = line.len();
    let mut content_length = 0usize;
    loop {
        line.clear();
        let read = reader.read_line(&mut line)?;
        header_bytes += read;
        if header_bytes > MAX_HEADER_BYTES {
            return Err(message("headers too large".into()));
        }
        let trimmed = line.trim_end();
        if read == 0 || trimmed.is_empty() {
            break;
        }
        let Some((name, value)) = trimmed.split_once(':') else {
            continue;
        };
        let value = value.trim();
        if name.eq_ignore_ascii_case("content-length") {
            content_length = value
                .parse()
                .map_err(|_| message("bad content-length".into()))?;
        } else if name.eq_ignore_ascii_case("authorization") {
            request.authorization = Some(value.to_string());
        }
    }
    if content_length > MAX_BODY_BYTES {
        return Err(message("body too large".into()));
    }
    request.body = vec![0; content_length];
    reader.read_exact(&mut request.body)?;
    Ok(request)
}

/// One request in, one status and JSON body out. `merged` is `limits.json`
/// as it stands, read fresh for every request.
pub fn handle(request: &Request, key: &str, merged: Option<&Merged>) -> (u16, Value) {
    if !authorized(request.authorization.as_deref(), key) {
        return (401, json!({ "error": "missing or wrong bearer key" }));
    }
    let path = request.path.split('?').next().unwrap_or_default();
    let accounts: &[AccountSnapshot] = merged.map(|m| m.accounts.as_slice()).unwrap_or(&[]);
    match (request.method.as_str(), path) {
        ("GET", "/v0/management/auth-files") => (200, auth_files(accounts)),
        ("POST", "/v0/management/api-call") => {
            match serde_json::from_slice::<Value>(&request.body) {
                Ok(call) => (200, api_call(&call, accounts)),
                Err(_) => (400, json!({ "error": "api-call body is not JSON" })),
            }
        }
        ("POST", "/v0/management/reset-quota") => (
            NOT_SUPPORTED,
            json!({ "error": "not supported: ovm limits serve is read-only" }),
        ),
        _ => (404, json!({ "error": "not found" })),
    }
}

fn authorized(header: Option<&str>, key: &str) -> bool {
    let Some(given) = header.and_then(|h| h.strip_prefix("Bearer ")) else {
        return false;
    };
    // Constant time over the key's length, so a wrong guess says nothing
    // about how much of it was right.
    let (given, key) = (given.trim().as_bytes(), key.as_bytes());
    given.len() == key.len() && given.iter().zip(key).fold(0u8, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// Every account in `limits.json`, as the hub's credential files. The id
/// and `auth_index` are the ovm account id; `email` carries the label, the
/// name the person gave the account, because that is the field a client
/// shows. Codex accounts name their ChatGPT account and plan the way an
/// id-token would.
pub fn auth_files(accounts: &[AccountSnapshot]) -> Value {
    let files: Vec<Value> = accounts
        .iter()
        .map(|account| {
            let mut file = json!({
                "id": account.id,
                "auth_index": account.id,
                "provider": provider_name(account.provider),
                "email": account.label.clone().unwrap_or_else(|| account.id.clone()),
            });
            if account.provider == Provider::Codex {
                let mut token = serde_json::Map::new();
                if let Some(id) = &account.account_id {
                    token.insert("chatgpt_account_id".into(), json!(id));
                }
                if let Some(plan) = &account.plan {
                    token.insert("chatgpt_plan_type".into(), json!(plan));
                }
                file["id_token"] = Value::Object(token);
            }
            file
        })
        .collect();
    json!({ "files": files })
}

fn provider_name(provider: Provider) -> &'static str {
    match provider {
        Provider::Claude => "claude",
        Provider::Codex => "codex",
    }
}

/// The `{status_code, body}` envelope for one proxied call. Only a GET of
/// the usage URL matching the account's provider is answered; every other
/// URL — reset credits and their consumption included — is refused with
/// 501, which the client reports as the provider refusing.
pub fn api_call(call: &Value, accounts: &[AccountSnapshot]) -> Value {
    let index = call.get("auth_index").and_then(Value::as_str).unwrap_or("");
    let method = call.get("method").and_then(Value::as_str).unwrap_or("GET");
    let url = call.get("url").and_then(Value::as_str).unwrap_or("");
    let Some(account) = accounts.iter().find(|a| a.id == index) else {
        return envelope(404, &json!({ "error": "no such account" }));
    };
    let wanted = match account.provider {
        Provider::Claude => CLAUDE_USAGE_URL,
        Provider::Codex => CODEX_USAGE_URL,
    };
    if !method.eq_ignore_ascii_case("GET") || url != wanted {
        return envelope(
            NOT_SUPPORTED,
            &json!({ "error": "not supported: ovm limits serve only answers usage reads" }),
        );
    }
    if let Some(error) = &account.error {
        return envelope(
            503,
            &json!({ "error": format!("last poll failed: {error}") }),
        );
    }
    let body = match account.provider {
        Provider::Claude => claude_usage(account),
        Provider::Codex => codex_usage(account),
    };
    envelope(200, &body)
}

fn envelope(status: u16, body: &Value) -> Value {
    json!({ "status_code": status, "body": body.to_string() })
}

fn window<'a>(account: &'a AccountSnapshot, id: &str) -> Option<&'a Window> {
    account.windows.iter().find(|w| w.id == id)
}

/// Claude's `/api/oauth/usage` shape: utilization in percent, reset as an
/// RFC 3339 string.
pub fn claude_usage(account: &AccountSnapshot) -> Value {
    let claude_window = |id: &str| {
        window(account, id).map_or(Value::Null, |w| {
            json!({
                "utilization": w.used_percent,
                "resets_at": w.resets_at.map(rfc3339),
            })
        })
    };
    json!({
        "five_hour": claude_window("five_hour"),
        "seven_day": claude_window("seven_day"),
    })
}

/// Codex's `wham/usage` shape: the `codex` limit's primary and secondary
/// windows, reset in epoch seconds.
pub fn codex_usage(account: &AccountSnapshot) -> Value {
    let codex_window = |id: &str| {
        window(account, id).map_or(Value::Null, |w| {
            let mut out = json!({ "used_percent": w.used_percent, "reset_at": w.resets_at });
            if let Some(minutes) = w.window_minutes {
                out["limit_window_seconds"] = json!(minutes * 60);
            }
            out
        })
    };
    json!({
        "plan_type": account.plan,
        "rate_limit": {
            "primary_window": codex_window("codex.primary"),
            "secondary_window": codex_window("codex.secondary"),
        },
    })
}

/// `YYYY-MM-DDTHH:MM:SSZ` for epoch seconds (civil-from-days; no date crate).
pub fn rfc3339(secs: u64) -> String {
    let secs = secs as i64;
    let days = secs.div_euclid(86_400);
    let of_day = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
        of_day / 3_600,
        (of_day % 3_600) / 60,
        of_day % 60
    )
}

fn reason(status: u16) -> &'static str {
    match status {
        200 => "OK",
        400 => "Bad Request",
        401 => "Unauthorized",
        404 => "Not Found",
        501 => "Not Implemented",
        _ => "Error",
    }
}

fn message(text: String) -> LimitsError {
    LimitsError::Message(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "k3y";

    fn account(provider: Provider, id: &str, label: &str, windows: Vec<Window>) -> AccountSnapshot {
        let mut snap = AccountSnapshot::empty(provider, id, Some(label.into()), "test");
        snap.windows = windows;
        snap
    }

    fn win(id: &str, used: f64, resets_at: u64, minutes: u64) -> Window {
        Window {
            id: id.into(),
            label: id.into(),
            used_percent: used,
            resets_at: Some(resets_at),
            window_minutes: Some(minutes),
            last_reset_at: None,
        }
    }

    fn fixture() -> Vec<AccountSnapshot> {
        let claude = account(
            Provider::Claude,
            "claude-1",
            "simcity",
            vec![
                win("five_hour", 2.0, 1_790_671_200, 300),
                win("seven_day", 53.0, 1_790_888_400, 10_080),
            ],
        );
        let mut codex = account(
            Provider::Codex,
            "codex-1",
            "codex",
            vec![win("codex.primary", 14.0, 1_791_124_873, 10_080)],
        );
        codex.plan = Some("pro".into());
        codex.account_id = Some("acct-1".into());
        vec![claude, codex]
    }

    fn request(method: &str, path: &str, body: Value) -> Request {
        Request {
            method: method.into(),
            path: path.into(),
            authorization: Some(format!("Bearer {KEY}")),
            body: body.to_string().into_bytes(),
        }
    }

    fn merged(accounts: Vec<AccountSnapshot>) -> Merged {
        Merged {
            schema: snapshot::SCHEMA.into(),
            generated_at: 0,
            host: "test".into(),
            last_poll_at: None,
            next_poll_at: None,
            interval_minutes: 60,
            accounts,
            events: vec![],
        }
    }

    fn body_of(envelope: &Value) -> Value {
        serde_json::from_str(envelope["body"].as_str().unwrap()).unwrap()
    }

    #[test]
    fn every_call_needs_the_key() {
        let mut req = request("GET", "/v0/management/auth-files", Value::Null);
        req.authorization = None;
        assert_eq!(handle(&req, KEY, None).0, 401);
        req.authorization = Some("Bearer k3z".into());
        assert_eq!(handle(&req, KEY, None).0, 401);
        req.authorization = Some("Bearer k3y".into());
        assert_eq!(handle(&req, KEY, None).0, 200);
    }

    #[test]
    fn auth_files_lists_accounts_with_their_labels() {
        let m = merged(fixture());
        let (status, body) = handle(
            &request("GET", "/v0/management/auth-files", Value::Null),
            KEY,
            Some(&m),
        );
        assert_eq!(status, 200);
        let files = body["files"].as_array().unwrap();
        assert_eq!(files[0]["provider"], "claude");
        assert_eq!(files[0]["auth_index"], "claude-1");
        assert_eq!(files[0]["email"], "simcity");
        assert!(files[0].get("id_token").is_none());
        assert_eq!(files[1]["provider"], "codex");
        assert_eq!(files[1]["id_token"]["chatgpt_account_id"], "acct-1");
        assert_eq!(files[1]["id_token"]["chatgpt_plan_type"], "pro");
    }

    #[test]
    fn claude_usage_reads_like_the_oauth_endpoint() {
        let call = json!({ "auth_index": "claude-1", "method": "GET", "url": CLAUDE_USAGE_URL });
        let out = api_call(&call, &fixture());
        assert_eq!(out["status_code"], 200);
        let body = body_of(&out);
        assert_eq!(body["five_hour"]["utilization"], 2.0);
        assert_eq!(body["seven_day"]["utilization"], 53.0);
        assert_eq!(body["seven_day"]["resets_at"], "2026-10-01T21:00:00Z");
    }

    #[test]
    fn codex_usage_reads_like_wham() {
        let call = json!({ "auth_index": "codex-1", "method": "GET", "url": CODEX_USAGE_URL });
        let body = body_of(&api_call(&call, &fixture()));
        assert_eq!(body["plan_type"], "pro");
        let primary = &body["rate_limit"]["primary_window"];
        assert_eq!(primary["used_percent"], 14.0);
        assert_eq!(primary["reset_at"], 1_791_124_873u64);
        assert_eq!(primary["limit_window_seconds"], 604_800);
        assert!(body["rate_limit"]["secondary_window"].is_null());
    }

    #[test]
    fn anything_but_a_usage_read_is_refused() {
        let accounts = fixture();
        let credits = json!({ "auth_index": "codex-1", "method": "GET",
            "url": "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits" });
        assert_eq!(api_call(&credits, &accounts)["status_code"], 501);
        let consume = json!({ "auth_index": "codex-1", "method": "POST",
            "url": "https://chatgpt.com/backend-api/wham/rate-limit-reset-credits/consume" });
        assert_eq!(api_call(&consume, &accounts)["status_code"], 501);
        // The usage URL of the other provider is not this account's.
        let crossed = json!({ "auth_index": "claude-1", "method": "GET", "url": CODEX_USAGE_URL });
        assert_eq!(api_call(&crossed, &accounts)["status_code"], 501);
        let unknown = json!({ "auth_index": "claude-9", "method": "GET", "url": CLAUDE_USAGE_URL });
        assert_eq!(api_call(&unknown, &accounts)["status_code"], 404);
        let reset = request(
            "POST",
            "/v0/management/reset-quota",
            json!({ "auth_index": "codex-1" }),
        );
        assert_eq!(handle(&reset, KEY, None).0, 501);
    }

    #[test]
    fn a_failed_poll_is_not_served_as_numbers() {
        let mut accounts = fixture();
        accounts[0].error = Some("timed out".into());
        let call = json!({ "auth_index": "claude-1", "method": "GET", "url": CLAUDE_USAGE_URL });
        assert_eq!(api_call(&call, &accounts)["status_code"], 503);
    }

    #[test]
    fn binds_only_to_loopback_or_the_tailnet() {
        assert_eq!(Bind::parse("local").unwrap(), Bind::Local(DEFAULT_PORT));
        assert_eq!(Bind::parse("tailnet:9000").unwrap(), Bind::Tailnet(9000));
        assert!(Bind::parse("100.101.102.103:8317").is_ok());
        assert!(Bind::parse("[fd7a:115c:a1e0::1]:8317").is_ok());
        assert!(Bind::parse("0.0.0.0:8317").is_err());
        assert!(Bind::parse("192.168.1.2:8317").is_err());
        assert!(Bind::parse("100.128.0.1:8317").is_err());
    }

    #[test]
    fn rfc3339_matches_known_instants() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_709_164_800), "2024-02-29T00:00:00Z");
        assert_eq!(rfc3339(1_790_888_400), "2026-10-01T21:00:00Z");
    }

    #[test]
    fn key_is_made_once_and_private() {
        use std::os::unix::fs::PermissionsExt;
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        let first = load_or_create_key(&dirs).unwrap();
        assert_eq!(first.len(), KEY_BYTES * 2);
        assert_eq!(load_or_create_key(&dirs).unwrap(), first);
        let mode = std::fs::metadata(key_path(&dirs))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
    }

    #[test]
    fn a_trickling_request_is_cut_off_at_the_deadline() {
        use std::net::TcpListener;
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = std::thread::spawn(move || {
            let mut stream = TcpStream::connect(addr).unwrap();
            // Headers that never end, one byte at a time.
            for _ in 0..10 {
                if stream.write_all(b"x").is_err() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(300));
            }
        });
        let (stream, _) = listener.accept().unwrap();
        let started = Instant::now();
        let limit = Duration::from_secs(1);
        assert!(read_request(&stream, limit).is_err());
        let took = started.elapsed();
        assert!(took >= limit && took < limit * 3, "{took:?}");
        drop(stream);
        client.join().unwrap();
    }
}
