//! CLIProxyAPI process supervision. The proxy is a self-contained Go binary;
//! claudex's only relationship with it is spawn/health-check/stop plus the
//! generated YAML config. It keeps running between sessions so repeat
//! launches skip straight through.

use crate::config::ClaudexConfig;
use crate::paths::{display, ClaudexDirs};
use crate::{ClaudexError, Result};
use console::style;
use fs4::{FileExt, TryLockError};
use std::fs::{File, OpenOptions};
use std::net::{Ipv4Addr, SocketAddrV4, TcpStream};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const SPAWN_WAIT_ATTEMPTS: u32 = 40;
const SPAWN_WAIT_INTERVAL: Duration = Duration::from_millis(250);

fn spawn_wait_attempts() -> u32 {
    std::env::var("OVM_CLAUDEX_PROXY_START_ATTEMPTS")
        .ok()
        .and_then(|value| value.parse().ok())
        .filter(|attempts| *attempts > 0)
        .unwrap_or(SPAWN_WAIT_ATTEMPTS)
}

/// Lifetime guard for a launched Claude process. Live sessions hold a shared
/// OS lock; proxy activation requires the exclusive form, so an update can
/// never restart the shared sidecar underneath another guarded session.
pub struct SessionGuard {
    file: File,
    exclusive: bool,
}

impl SessionGuard {
    pub fn acquire(dirs: &ClaudexDirs) -> Result<Self> {
        std::fs::create_dir_all(dirs.proxy_dir())?;
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(dirs.proxy_sessions_lock())?;
        match FileExt::try_lock(&file) {
            Ok(()) => Ok(Self {
                file,
                exclusive: true,
            }),
            Err(TryLockError::WouldBlock) => {
                FileExt::lock_shared(&file)?;
                Ok(Self {
                    file,
                    exclusive: false,
                })
            }
            Err(TryLockError::Error(error)) => Err(error.into()),
        }
    }

    pub fn is_exclusive(&self) -> bool {
        self.exclusive
    }

    /// `flock(LOCK_SH)` atomically converts the exclusive lock on supported
    /// Unix platforms, closing the gap where another launcher could activate
    /// an update before this session has joined the shared lease.
    pub fn downgrade(mut self) -> Result<Self> {
        if self.exclusive {
            FileExt::lock_shared(&self.file)?;
            self.exclusive = false;
        }
        Ok(self)
    }

    /// Keep the shared lock across the first `exec` into OVM. The descriptor
    /// number is passed through a private environment variable so OVM can
    /// immediately restore close-on-exec before it spawns Claude or helpers.
    #[cfg(unix)]
    pub fn make_inheritable(&self) -> Result<i32> {
        use std::os::fd::AsRawFd;

        let fd = self.file.as_raw_fd();
        // SAFETY: `fd` belongs to `self.file` and remains open for this call.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        // SAFETY: same valid descriptor; only FD_CLOEXEC is cleared.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(std::io::Error::last_os_error().into());
        }
        Ok(fd)
    }
}

/// Where the proxy binary came from — shown in the launch banner and doctor.
pub enum ProxyBinary {
    /// OVM-managed install; the string is the version directory name.
    Managed { path: PathBuf, version: String },
    /// Found on PATH (e.g. Homebrew) — the pre-managed-install fallback.
    System { path: PathBuf },
}

impl ProxyBinary {
    pub fn path(&self) -> &PathBuf {
        match self {
            ProxyBinary::Managed { path, .. } => path,
            ProxyBinary::System { path } => path,
        }
    }

    pub fn version_label(&self) -> String {
        match self {
            ProxyBinary::Managed { version, .. } => version.clone(),
            ProxyBinary::System { .. } => "system".into(),
        }
    }
}

/// Locate the proxy binary. Order: pinned managed version, `current` symlink,
/// then PATH — deterministic first, so a stray PATH entry can't shadow a
/// managed install.
///
/// A pin is a contract: when one is set, ONLY the pinned version resolves.
/// Falling back to `current` or PATH would silently launch an unpinned proxy
/// while the banner claims otherwise.
pub fn resolve_binary(dirs: &ClaudexDirs, config: &ClaudexConfig) -> Option<ProxyBinary> {
    if let Some(pin) = &config.pin {
        // The pin becomes a path component — refuse separators/traversal.
        if pin.proxy.contains('/') || pin.proxy.contains("..") {
            return None;
        }
        let pinned = dirs
            .proxy_versions_dir()
            .join(&pin.proxy)
            .join("cliproxyapi");
        return pinned.is_file().then(|| ProxyBinary::Managed {
            path: pinned,
            version: pin.proxy.clone(),
        });
    }

    let current = dirs.proxy_current();
    if let Ok(target) = std::fs::canonicalize(&current) {
        if target.is_file() {
            let version = target
                .parent()
                .and_then(|dir| dir.file_name())
                .map(|name| name.to_string_lossy().into_owned())
                .unwrap_or_else(|| "unknown".into());
            return Some(ProxyBinary::Managed {
                path: target,
                version,
            });
        }
    }

    find_on_path("cliproxyapi").map(|path| ProxyBinary::System { path })
}

fn find_on_path(name: &str) -> Option<PathBuf> {
    let path_env = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_env) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

/// What an identity probe of the proxy port found.
#[derive(Debug, PartialEq, Eq)]
pub enum ProxyProbe {
    /// Answered `/v1/models` with our key: it's our proxy, safe to use.
    Verified,
    /// Something is listening but it isn't our proxy (accepts an arbitrary
    /// key, rejects our key, speaks the wrong protocol, or a *different*
    /// process owns the listening socket). Claude traffic and the local key
    /// must never be sent to it.
    ForeignListener(String),
    /// Something is listening and rejected the canary, but claudex could NOT
    /// positively confirm the listener is its own proxy: no verifiable pidfile,
    /// `ps` was unavailable, or `lsof` is missing/unparseable. The configured
    /// key was NOT transmitted. Kept distinct from `ForeignListener` so the
    /// caller can advise `ovm claudex stop` / restart instead of implying a
    /// hostile squatter.
    Unverified(String),
    /// Nothing is accepting connections on the port.
    Down,
}

/// How [`probe`] proves the listener is our own proxy before the *real* key
/// crosses the socket. Canary rejection alone is never treated as identity.
pub enum ProbeIdentity<'a> {
    /// Positive identity comes from the proxy pidfile: it must exist, pass the
    /// PID-identity check (command name + start time), and its pid must
    /// currently own the LISTENING socket on the probed port. Every production
    /// caller uses this — the pidfile is written the instant claudex spawns the
    /// proxy, so it is present for both freshly-spawned and adopted daemons.
    Pidfile(&'a ClaudexDirs),
    /// Test hook: skip socket-owner verification so unit tests can drive the
    /// HTTP state machine against an in-process fake listener. Never
    /// constructed in production code.
    #[cfg(test)]
    TrustForTest,
}

/// Whether the listener on the port could be positively tied to our proxy.
enum ListenerIdentity {
    /// Our verified proxy pid owns the listening socket — safe to send the key.
    Confirmed,
    /// A *different* process owns the socket, or the recorded pid no longer
    /// belongs to our proxy: definitively not ours.
    Foreign(String),
    /// Identity could not be determined (no pidfile, `ps` failed, or `lsof`
    /// missing/unparseable). The key must not be sent.
    Unverified(String),
}

/// Raw localhost HTTP/1.1 GET of `/v1/models` (no TLS, no client dep).
/// Returns (status, body) or None when nothing accepts the connection.
fn fetch_models(port: u16, api_key: &str) -> Option<(String, String)> {
    let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
    let mut stream = TcpStream::connect_timeout(&addr.into(), Duration::from_millis(300)).ok()?;
    let _ = stream.set_read_timeout(Some(Duration::from_secs(3)));
    let _ = stream.set_write_timeout(Some(Duration::from_secs(3)));

    let request = format!(
        "GET /v1/models HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nAuthorization: Bearer {api_key}\r\nConnection: close\r\n\r\n"
    );
    use std::io::{Read, Write};
    if stream.write_all(request.as_bytes()).is_err() {
        return Some((String::new(), String::new()));
    }
    let mut response = Vec::new();
    // Cap the read so a hostile listener can't feed us unbounded data.
    let _ = stream.take(256 * 1024).read_to_end(&mut response);
    let response = String::from_utf8_lossy(&response);

    let status = response
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or("")
        .to_string();
    let body = response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body.to_string())
        .unwrap_or_default();
    Some((status, body))
}

/// Identity check in three steps. The guarantee this function upholds:
/// **the configured key is NEVER transmitted to a listener whose identity we
/// could not positively confirm.**
///
/// 1. **Canary**: request with a RANDOM key. A genuine CLIProxyAPI must
///    reject it (401/403). A listener that accepts anything is flagged
///    foreign before our real key ever crosses the socket.
/// 2. **Ownership**: canary rejection alone proves nothing (any squatter can
///    401). Before the real key is sent we demand positive identity via
///    [`ProbeIdentity`]: the listening socket on `port` must be owned by our
///    verified proxy pid (pidfile present, PID-identity check passed, and
///    `lsof` confirms that pid owns the LISTEN socket). Anything short of that
///    yields `Foreign`/`Unverified` and the key is withheld.
/// 3. **Auth**: only once ownership is confirmed do we send the real key,
///    which must return a model list.
///
/// Residual risk (documented): a same-user attacker who forges the pidfile AND
/// runs a process named `cliproxyapi` that owns the port is indistinguishable
/// from our proxy — but such an attacker can already read the key file
/// directly, so the boundary this defends is other *users* (or unrelated
/// same-user processes) squatting the port, none of which can satisfy the
/// socket-ownership check.
pub fn probe(port: u16, api_key: &str, identity: ProbeIdentity) -> ProxyProbe {
    let canary = format!("canary-{}", std::process::id());
    match fetch_models(port, &canary) {
        None => return ProxyProbe::Down,
        Some((status, _)) => match status.as_str() {
            // Both are key-rejections; which one a CLIProxyAPI build sends
            // is an implementation detail. The property that matters is
            // "does NOT accept an arbitrary key".
            "401" | "403" => {}
            "" => return ProxyProbe::ForeignListener("no HTTP response".into()),
            other => {
                return ProxyProbe::ForeignListener(format!(
                    "accepted an invalid key (HTTP {other}) — not our proxy"
                ))
            }
        },
    }

    // The canary was rejected, which a genuine CLIProxyAPI does — but so does
    // any squatter that simply 401s. Demand socket ownership before the REAL
    // key crosses the wire; canary rejection is never treated as identity.
    match check_listener_identity(port, &identity) {
        ListenerIdentity::Confirmed => {}
        ListenerIdentity::Foreign(why) => return ProxyProbe::ForeignListener(why),
        ListenerIdentity::Unverified(why) => return ProxyProbe::Unverified(why),
    }

    // KNOWN RESIDUAL (pre-1.0 backlog): the `lsof` ownership check above and the
    // key transmission below are two separate operations over TCP; they cannot
    // be made atomic. On a shared host a sub-second race — our verified proxy
    // exits and another same-user process rebinds the port between the check and
    // the send — could still receive the real key. The real fix is a
    // Unix-domain-socket handoff (kernel-attested peer credentials) or a
    // signed-nonce challenge so identity and key exchange share one connection.
    let Some((status, body)) = fetch_models(port, api_key) else {
        return ProxyProbe::Down;
    };
    match status.as_str() {
        "200" if body.contains("\"data\"") => ProxyProbe::Verified,
        "200" => ProxyProbe::ForeignListener("HTTP 200 but not a model list".into()),
        "401" | "403" => ProxyProbe::ForeignListener(format!(
            "HTTP {status} — rejects the configured key (foreign proxy or stale config)"
        )),
        "" => ProxyProbe::ForeignListener("no HTTP response".into()),
        other => ProxyProbe::ForeignListener(format!("unexpected HTTP {other}")),
    }
}

/// Confirm the listener on `port` is our proxy before the key is transmitted.
/// The pidfile identifies the pid, `pid_matches_record` proves that pid is
/// still our proxy process, and `lsof` proves that pid owns the LISTEN socket.
fn check_listener_identity(port: u16, identity: &ProbeIdentity) -> ListenerIdentity {
    let dirs = match identity {
        ProbeIdentity::Pidfile(dirs) => *dirs,
        #[cfg(test)]
        ProbeIdentity::TrustForTest => return ListenerIdentity::Confirmed,
    };

    let Some(record) = read_pid_record(&dirs.proxy_pid_file()) else {
        return ListenerIdentity::Unverified(
            "no proxy pidfile, so the listener on the port cannot be confirmed as claudex's proxy"
                .into(),
        );
    };
    match pid_matches_record(&record) {
        Some(true) => {}
        Some(false) => {
            return ListenerIdentity::Foreign(
                "the recorded proxy pid no longer belongs to claudex's proxy".into(),
            )
        }
        None => {
            return ListenerIdentity::Unverified(
                "could not verify the proxy pid (`ps` failed)".into(),
            )
        }
    }

    match pid_owns_listen_socket(record.pid, port) {
        Ok(true) => ListenerIdentity::Confirmed,
        Ok(false) => ListenerIdentity::Foreign(format!(
            "pid {} does not own the listening socket on 127.0.0.1:{port}",
            record.pid
        )),
        // `lsof` missing or unparseable: we cannot prove ownership, so we
        // refuse to send the key rather than trust the canary rejection.
        Err(why) => ListenerIdentity::Unverified(why),
    }
}

/// `lsof` from a known absolute location if present, else via PATH. A launcher
/// with a minimal PATH (or macOS, where lsof lives in `/usr/sbin`) still finds
/// it.
fn lsof_command() -> Command {
    for path in ["/usr/sbin/lsof", "/usr/bin/lsof", "/sbin/lsof", "/bin/lsof"] {
        if std::path::Path::new(path).exists() {
            return Command::new(path);
        }
    }
    Command::new("lsof")
}

/// Does `pid` own a LISTENING TCP socket on `port`? Parses `lsof -F` field
/// output (`p<pid>` lines), which is stable across macOS and Linux. The
/// `-sTCP:LISTEN` filter guarantees every reported socket is in LISTEN state on
/// the local `port`, so a matching pid definitively owns the listener.
///
/// `Ok(_)` means lsof ran and answered; `Err` means lsof is absent or its
/// output was unparseable — the caller must then treat identity as UNVERIFIED
/// and refuse to transmit the key.
fn pid_owns_listen_socket(pid: u32, port: u16) -> std::result::Result<bool, String> {
    let output = lsof_command()
        .args(["-nP", &format!("-iTCP:{port}"), "-sTCP:LISTEN", "-Fp"])
        .output()
        .map_err(|error| {
            format!(
                "could not run `lsof` to confirm claudex's proxy owns port {port} ({error}); \
                 run `ovm claudex stop` and relaunch"
            )
        })?;
    // lsof exits non-zero when nothing matches the filter; that is a valid
    // "no LISTEN socket on this port" answer (owned = false), not a tool
    // failure. Only a `p` line we cannot parse is treated as unparseable.
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut owned = false;
    for line in stdout.lines() {
        let Some(rest) = line.strip_prefix('p') else {
            continue;
        };
        match rest.trim().parse::<u32>() {
            Ok(found) if found == pid => owned = true,
            Ok(_) => {}
            Err(_) => {
                return Err(format!(
                    "could not parse `lsof` output while confirming port {port}; \
                     run `ovm claudex stop` and relaunch"
                ))
            }
        }
    }
    Ok(owned)
}

/// The model ids a verified proxy exposes (aliases included). None when the
/// proxy is down or unverified.
pub fn list_models(port: u16, api_key: &str) -> Option<Vec<String>> {
    let (status, body) = fetch_models(port, api_key)?;
    if status != "200" {
        return None;
    }
    let parsed: serde_json::Value = serde_json::from_str(&body).ok()?;
    Some(
        parsed["data"]
            .as_array()?
            .iter()
            .filter_map(|model| model["id"].as_str().map(str::to_string))
            .collect(),
    )
}

fn port_collision_error(port: u16, why: &str) -> ClaudexError {
    ClaudexError::Message(format!(
        "Port 127.0.0.1:{port} is occupied by something that isn't claudex's proxy ({why}). \
         Refusing to send traffic or credentials to it. Stop that process, or change \
         proxy.port in ~/.ovm/claudex/config.json and re-run: ovm claudex setup"
    ))
}

/// Something is listening and rejected the canary, but claudex could not prove
/// it owns the port. The key was withheld; the daemon may simply predate the
/// pidfile, or `lsof`/`ps` were unavailable — so guide a clean restart rather
/// than implying a hostile squatter.
fn unverified_identity_error(port: u16, why: &str) -> ClaudexError {
    ClaudexError::Message(format!(
        "Something is listening on 127.0.0.1:{port}, but claudex could not confirm it is its own \
         proxy ({why}). Refusing to send the proxy key to an unverified listener. Run \
         `ovm claudex stop` and relaunch; if the listener is not claudex's proxy, stop it or \
         change proxy.port in ~/.ovm/claudex/config.json and re-run: ovm claudex setup"
    ))
}

/// Session launches call this while holding the session lock. The pid record
/// remembers that fact so future automatic updates know the running daemon
/// cannot have untracked pre-upgrade clients.
pub fn ensure_running_for_session(
    dirs: &ClaudexDirs,
    config: &ClaudexConfig,
) -> Result<Option<ProxyBinary>> {
    ensure_running_with_session_guard(dirs, config, true)
}

/// The only environment variables the CLIProxyAPI sidecar inherits. It is fully
/// config-file-driven, so this is the minimal set any process needs to run and
/// make outbound HTTPS calls to the upstream providers — none of these are
/// secrets:
/// - `PATH` to locate any helper it shells out to;
/// - `HOME`/`TMPDIR`/`TMP`/`TEMP` for path + scratch resolution;
/// - locale/timezone vars for correct formatting;
/// - `SSL_CERT_FILE`/`SSL_CERT_DIR` so a custom CA bundle still validates TLS;
/// - the standard proxy vars so a corporate egress proxy keeps working.
///
/// Everything else — provider credentials, cloud creds, `OVM_GITHUB_TOKEN`, and
/// any future ambient secret — is dropped by construction.
const SIDECAR_ENV_ALLOWLIST: &[&str] = &[
    "PATH",
    "HOME",
    "TMPDIR",
    "TMP",
    "TEMP",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "LANGUAGE",
    "TZ",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "NO_PROXY",
    "http_proxy",
    "https_proxy",
    "no_proxy",
];

/// Clear the inherited environment and re-add only [`SIDECAR_ENV_ALLOWLIST`].
fn apply_minimal_sidecar_env(command: &mut Command) {
    command.env_clear();
    for key in SIDECAR_ENV_ALLOWLIST {
        if let Some(value) = std::env::var_os(key) {
            command.env(key, value);
        }
    }
}

fn ensure_running_with_session_guard(
    dirs: &ClaudexDirs,
    config: &ClaudexConfig,
    session_guarded: bool,
) -> Result<Option<ProxyBinary>> {
    match probe(
        config.proxy.port,
        &config.proxy.api_key,
        ProbeIdentity::Pidfile(dirs),
    ) {
        ProxyProbe::Verified => {
            verify_running_matches_pin(dirs, config)?;
            return Ok(None);
        }
        ProxyProbe::ForeignListener(why) => {
            return Err(port_collision_error(config.proxy.port, &why))
        }
        ProxyProbe::Unverified(why) => {
            return Err(unverified_identity_error(config.proxy.port, &why))
        }
        ProxyProbe::Down => {}
    }

    let binary = resolve_binary(dirs, config).ok_or_else(|| match &config.pin {
        Some(pin) => ClaudexError::Message(format!(
            "Pinned proxy version {} is not installed under ~/.ovm/claudex/proxy/versions/. \
             Install it, or clear the pin in ~/.ovm/claudex/config.json.",
            pin.proxy
        )),
        None => ClaudexError::Message(
            "CLIProxyAPI not found. Run: ovm claudex setup (or `brew install cliproxyapi`).".into(),
        ),
    })?;

    let proxy_config = dirs.proxy_config_file();
    if !proxy_config.is_file() {
        return Err(ClaudexError::Message(
            "Proxy config missing. Run: ovm claudex setup".into(),
        ));
    }

    let log_path = dirs.proxy_log_file();
    if let Some(parent) = log_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut log_opts = std::fs::OpenOptions::new();
    log_opts.create(true).append(true);
    // The proxy log can capture upstream request/response bodies and bearer
    // tokens — create it owner-only, never the default 0644.
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        log_opts.mode(0o600);
    }
    let log = log_opts.open(&log_path)?;
    let log_err = log.try_clone()?;

    eprintln!(
        "  {} Starting cliproxyapi on 127.0.0.1:{}…",
        style("→").dim(),
        config.proxy.port
    );

    let mut command = Command::new(binary.path());
    command
        .arg("--config")
        .arg(&proxy_config)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));
    // The sidecar's behavior is fully driven by its config file, so it inherits a
    // curated allowlist rather than the full environment minus a denylist. This
    // makes it impossible for an ambient secret (a provider key, OVM_GITHUB_TOKEN,
    // cloud credentials, …) to reach the long-lived proxy just because it happens
    // to be in the launching shell's environment.
    apply_minimal_sidecar_env(&mut command);
    let mut child = command.spawn()?;

    // A failed record write must never orphan the child we just started.
    if let Err(error) = write_pid_record(dirs, child.id(), binary.path(), session_guarded) {
        reap_failed_spawn(dirs, &mut child);
        return Err(error);
    }

    // Wait for a VERIFIED identity, not just an open port: our key must be
    // accepted, which also catches a config/key mismatch right at startup.
    let wait_attempts = spawn_wait_attempts();
    for _ in 0..wait_attempts {
        match probe(
            config.proxy.port,
            &config.proxy.api_key,
            ProbeIdentity::Pidfile(dirs),
        ) {
            ProxyProbe::Verified => return Ok(Some(binary)),
            ProxyProbe::ForeignListener(why) if why.starts_with("HTTP 401") => {
                reap_failed_spawn(dirs, &mut child);
                return Err(ClaudexError::Message(format!(
                    "cliproxyapi started but rejects the configured key ({why}). \
                     The proxy config and claudex config have drifted — re-run: ovm claudex setup"
                )));
            }
            // Startup race: the port can accept before routes are ready.
            // Keep polling until the deadline.
            _ => {}
        }
        // A child that has already exited can never come up — fail now
        // instead of burning the rest of the poll budget.
        if let Ok(Some(status)) = child.try_wait() {
            reap_failed_spawn(dirs, &mut child);
            return Err(ClaudexError::Message(format!(
                "cliproxyapi exited during startup ({status}) — see {}",
                display(&log_path)
            )));
        }
        std::thread::sleep(SPAWN_WAIT_INTERVAL);
    }

    // Never leave an orphan half-started proxy (plus its stale pidfile)
    // behind a startup failure.
    reap_failed_spawn(dirs, &mut child);
    Err(ClaudexError::Message(format!(
        "cliproxyapi did not come up verified on 127.0.0.1:{} within {:.0?} — see {}",
        config.proxy.port,
        SPAWN_WAIT_INTERVAL * wait_attempts,
        display(&log_path)
    )))
}

fn verify_running_matches_pin(dirs: &ClaudexDirs, config: &ClaudexConfig) -> Result<()> {
    let Some(pin) = &config.pin else {
        return Ok(());
    };
    let desired = dirs
        .proxy_versions_dir()
        .join(&pin.proxy)
        .join("cliproxyapi");
    let desired = std::fs::canonicalize(&desired).map_err(|_| {
        ClaudexError::Message(format!(
            "Pinned proxy {} is not installed. Run: ovm claudex update {}",
            pin.proxy, pin.proxy
        ))
    })?;
    let running = read_pid_record(&dirs.proxy_pid_file())
        .filter(|record| pid_matches_record(record) == Some(true))
        .and_then(|record| std::fs::canonicalize(record.binary).ok());
    if running.as_ref() == Some(&desired) {
        return Ok(());
    }
    Err(ClaudexError::Message(format!(
        "The running proxy does not match pinned version {}. Close active claudex sessions, run `ovm claudex stop`, then relaunch.",
        pin.proxy
    )))
}

/// Kill and reap a child that failed to come up healthy; drop its pidfile.
fn reap_failed_spawn(dirs: &ClaudexDirs, child: &mut std::process::Child) {
    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(dirs.proxy_pid_file());
}

/// What the pidfile records: enough to verify process identity before ever
/// sending it a signal. PIDs get reused; a pid alone is not an identity —
/// `started` (unix seconds at spawn) is what defeats reuse.
#[derive(serde::Serialize, serde::Deserialize)]
struct PidRecord {
    pid: u32,
    binary: PathBuf,
    #[serde(default)]
    started: u64,
    /// True only when the daemon was started by a launcher that holds the
    /// shared session lock for every client lifetime.
    #[serde(default)]
    session_guarded: bool,
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_secs())
        .unwrap_or(0)
}

fn write_pid_record(
    dirs: &ClaudexDirs,
    pid: u32,
    binary: &std::path::Path,
    session_guarded: bool,
) -> Result<()> {
    let record = PidRecord {
        pid,
        binary: binary.to_path_buf(),
        started: now_unix(),
        session_guarded,
    };
    crate::config::write_atomic(
        &dirs.proxy_pid_file(),
        &serde_json::to_string(&record)?,
        None,
    )
}

/// Read the pidfile, tolerating the legacy bare-pid format.
fn read_pid_record(path: &std::path::Path) -> Option<PidRecord> {
    let contents = std::fs::read_to_string(path).ok()?;
    if let Ok(record) = serde_json::from_str::<PidRecord>(&contents) {
        return Some(record);
    }
    let pid: u32 = contents.trim().parse().ok()?;
    Some(PidRecord {
        pid,
        binary: PathBuf::from("cliproxyapi"),
        started: 0,
        session_guarded: false,
    })
}

/// Parse `ps` etime output (`[[dd-]hh:]mm:ss`) into seconds.
fn parse_etime(etime: &str) -> Option<u64> {
    let etime = etime.trim();
    let (days, rest) = match etime.split_once('-') {
        Some((days, rest)) => (days.parse::<u64>().ok()?, rest),
        None => (0, etime),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|part| part.parse::<u64>())
        .collect::<std::result::Result<_, _>>()
        .ok()?;
    let seconds = match parts.as_slice() {
        [hours, minutes, seconds] => hours * 3600 + minutes * 60 + seconds,
        [minutes, seconds] => minutes * 60 + seconds,
        _ => return None,
    };
    Some(days * 86_400 + seconds)
}

/// Outcome of querying one `ps` field for a pid. Distinguishes "the pid is
/// gone" from "`ps` could not run at all": the identity check guards a
/// `kill()`, so an unrunnable `ps` must fail closed (cannot verify) rather
/// than read as "process gone" and drop a live proxy's pidfile.
enum PsField {
    /// `ps` ran and reported this (trimmed) value for the pid.
    Value(String),
    /// `ps` ran but no process has that pid.
    NoProcess,
    /// `ps` could not be executed — identity cannot be determined.
    Unavailable,
}

fn ps_field(pid: u32, field: &str) -> PsField {
    match Command::new("ps")
        .args(["-p", &pid.to_string(), "-o", &format!("{field}=")])
        .output()
    {
        Err(_) => PsField::Unavailable,
        Ok(output) if output.status.success() => {
            PsField::Value(String::from_utf8_lossy(&output.stdout).trim().to_string())
        }
        Ok(_) => PsField::NoProcess,
    }
}

/// Does `pid` currently belong to the process we spawned? Checks the command
/// name AND (when recorded) that the process start time matches the spawn
/// time within tolerance — a reused PID fails the start-time comparison.
/// `comm` and `etime` are queried separately: in combined format macOS
/// truncates comm to a fixed column width, breaking path comparison.
fn pid_matches_record(record: &PidRecord) -> Option<bool> {
    let comm = match ps_field(record.pid, "comm") {
        PsField::Value(comm) => comm,
        // ps ran and the pid is gone — definitely not our process.
        PsField::NoProcess => return Some(false),
        // ps couldn't run — cannot verify identity; fail closed so the
        // caller leaves the pidfile untouched instead of signalling.
        PsField::Unavailable => return None,
    };

    let expected = record
        .binary
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "cliproxyapi".into());
    // macOS `ps -o comm` yields the full executable path; Linux yields the
    // kernel comm, truncated to 15 bytes. Match on the basename only — a bare
    // `comm.ends_with(expected)` would accept a foreign `/x/notcliproxyapi`.
    // 15 exactly: Linux TASK_COMM_LEN is 16 including NUL, so only a 15-byte
    // comm can be a truncation; longer strings are full names.
    let comm_base = comm.rsplit('/').next().unwrap_or(comm.as_str());
    let matches =
        comm_base == expected || (comm_base.len() == 15 && expected.starts_with(comm_base));
    if !matches {
        return Some(false);
    }

    // A zombie has already exited — it can't serve requests, hold the
    // port, or respond to another signal; only its unreaped entry
    // lingers (Linux `ps -o comm=` still reports the plain name, so the
    // name check above matches). Stop-wait loops must not spin on it.
    if let PsField::Value(state) = ps_field(record.pid, "stat") {
        if state.starts_with('Z') {
            return Some(false);
        }
    }

    if record.started > 0 {
        // Fail closed: a recorded start time that can't be verified means
        // "cannot confirm identity" — this path guards a kill(), so an
        // unparseable etime must never silently pass.
        let elapsed = match ps_field(record.pid, "etime") {
            PsField::Value(value) => parse_etime(&value)?,
            // ps failed, pid vanished, or unparseable — cannot verify the
            // start time, so fail closed rather than signal a stale match.
            _ => return None,
        };
        let implied_start = now_unix().saturating_sub(elapsed);
        let drift = implied_start.abs_diff(record.started);
        // 90s tolerance: coarse etime granularity + clock slew.
        return Some(drift <= 90);
    }
    Some(true)
}

fn version_from_managed_path(dirs: &ClaudexDirs, path: &std::path::Path) -> Option<String> {
    let path = std::fs::canonicalize(path).ok()?;
    let versions = std::fs::canonicalize(dirs.proxy_versions_dir()).ok()?;
    path.strip_prefix(versions)
        .ok()?
        .components()
        .next()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
}

/// The version actually serving requests, rather than the version named by
/// `current` (which may differ while an update is staged).
pub fn running_version_label(dirs: &ClaudexDirs, config: &ClaudexConfig) -> String {
    if probe(
        config.proxy.port,
        &config.proxy.api_key,
        ProbeIdentity::Pidfile(dirs),
    ) == ProxyProbe::Verified
    {
        if let Some(record) = read_pid_record(&dirs.proxy_pid_file()) {
            if pid_matches_record(&record) == Some(true) {
                return version_from_managed_path(dirs, &record.binary)
                    .unwrap_or_else(|| "system".into());
            }
        }
    }
    resolve_binary(dirs, config)
        .map(|binary| binary.version_label())
        .unwrap_or_else(|| "external".into())
}

/// Activate a prepared update while the caller owns the exclusive session
/// lock. Returns false when a legacy daemon could still have pre-lock clients;
/// in that case the verified update remains staged for an explicit stop or a
/// later daemon restart.
pub fn activate_pending_update(
    dirs: &ClaudexDirs,
    config: &ClaudexConfig,
    explicit: bool,
) -> Result<bool> {
    let Some(pending) = crate::install::load_pending_update(dirs)? else {
        return Ok(false);
    };
    if config.pin.is_some() {
        eprintln!(
            "  {} cliproxyapi {} is installed but remains staged because the Claude/proxy pair is pinned.",
            style("⚲").yellow(),
            style(&pending.version).green().bold()
        );
        return Ok(false);
    }
    if !explicit && pending.automatic && !config.auto_update_proxy {
        return Ok(false);
    }

    let expected = dirs
        .proxy_versions_dir()
        .join(&pending.version)
        .join("cliproxyapi");
    let expected = std::fs::canonicalize(expected).map_err(|error| {
        ClaudexError::Message(format!(
            "prepared cliproxyapi {} is missing: {error}",
            pending.version
        ))
    })?;
    let pending_binary = std::fs::canonicalize(&pending.binary)?;
    if pending_binary != expected {
        return Err(ClaudexError::Message(
            "pending proxy update points outside its managed version directory".into(),
        ));
    }

    let previous = std::fs::read_link(dirs.proxy_current()).ok();
    if probe(
        config.proxy.port,
        &config.proxy.api_key,
        ProbeIdentity::Pidfile(dirs),
    ) == ProxyProbe::Verified
    {
        let Some(record) = read_pid_record(&dirs.proxy_pid_file()) else {
            eprintln!(
                "  {} cliproxyapi {} is ready, but the running proxy has no verified pid record; update remains staged. Stop that proxy, then relaunch claudex.",
                style("!").yellow(),
                style(&pending.version).green().bold()
            );
            return Ok(false);
        };
        if pid_matches_record(&record) != Some(true) {
            eprintln!(
                "  {} cliproxyapi {} is ready, but the running proxy identity cannot be verified; update remains staged.",
                style("!").yellow(),
                style(&pending.version).green().bold()
            );
            return Ok(false);
        }
        if !record.session_guarded && !explicit {
            eprintln!(
                "  {} cliproxyapi {} is ready. This proxy predates safe session tracking, so it was not restarted automatically. After existing sessions exit, run: ovm claudex update",
                style("!").yellow(),
                style(&pending.version).green().bold()
            );
            return Ok(false);
        }
        if std::fs::canonicalize(&record.binary).ok().as_ref() == Some(&pending_binary) {
            crate::install::clear_pending_update(dirs)?;
            return Ok(false);
        }
        stop(dirs)?;
    }

    crate::install::switch_current(dirs, &pending_binary)?;
    match ensure_running_for_session(dirs, config) {
        Ok(_) => {
            crate::install::clear_pending_update(dirs)?;
            eprintln!(
                "  {} Activated and verified cliproxyapi {}.",
                style("✓").green(),
                style(&pending.version).green().bold()
            );
            Ok(true)
        }
        Err(error) => {
            if let Some(previous) = previous {
                crate::install::switch_current(dirs, &previous)?;
                let _ = ensure_running_for_session(dirs, config);
            }
            Err(ClaudexError::Message(format!(
                "cliproxyapi {} failed verification ({error}); rolled `current` back",
                pending.version
            )))
        }
    }
}

/// `ovm claudex stop` — terminate the background proxy via its pidfile,
/// verifying process identity first so a reused PID can never get our
/// signal.
pub fn stop_command() -> Result<()> {
    stop(&ClaudexDirs::new()?)
}

pub fn stop(dirs: &ClaudexDirs) -> Result<()> {
    stop_with_attempts(dirs, 20)
}

fn stop_with_attempts(dirs: &ClaudexDirs, wait_attempts: u32) -> Result<()> {
    let pid_file = dirs.proxy_pid_file();

    if !pid_file.exists() {
        eprintln!("  {} Proxy is not running (no pidfile).", style("—").dim());
        return Ok(());
    }
    let Some(record) = read_pid_record(&pid_file) else {
        std::fs::remove_file(&pid_file)?;
        return Err(ClaudexError::Message(
            "Stale pidfile had unexpected contents; removed it.".into(),
        ));
    };

    match pid_matches_record(&record) {
        Some(true) => {
            let status = Command::new("kill").arg(record.pid.to_string()).status()?;
            if status.success() {
                // Wait for actual exit so a follow-up start (e.g. `update`'s
                // restart-and-verify) can never probe the OLD process.
                let mut exited = false;
                for _ in 0..wait_attempts {
                    // Only a positive "no longer our process" ends the wait.
                    // A `None` (ps couldn't verify) must NOT count as exited —
                    // otherwise a transient ps failure here would delete a
                    // still-live proxy's pidfile. Keep waiting; if it never
                    // resolves to Some(false) we fall through to the
                    // did-not-exit error below, leaving the pidfile intact.
                    if pid_matches_record(&record) == Some(false) {
                        exited = true;
                        break;
                    }
                    std::thread::sleep(Duration::from_millis(250));
                }
                if exited {
                    std::fs::remove_file(&pid_file)?;
                    eprintln!(
                        "  {} Stopped cliproxyapi (pid {}).",
                        style("✓").green(),
                        record.pid
                    );
                } else {
                    return Err(ClaudexError::Message(format!(
                        "Sent SIGTERM to proxy pid {} but it did not exit within 5s; refusing to start another proxy",
                        record.pid
                    )));
                }
            } else {
                match pid_matches_record(&record) {
                    // Only a confirmed "not our process" justifies dropping the
                    // pidfile after a failed signal. A `None` here means ps
                    // couldn't verify — fail closed and keep the pidfile.
                    Some(false) => {
                        std::fs::remove_file(&pid_file)?;
                        eprintln!(
                            "  {} Process {} disappeared while stopping; cleaned up pidfile.",
                            style("—").dim(),
                            record.pid
                        );
                    }
                    Some(true) => {
                        return Err(ClaudexError::Message(format!(
                            "Could not signal proxy pid {}; leaving its verified pidfile intact",
                            record.pid
                        )));
                    }
                    None => {
                        return Err(ClaudexError::Message(format!(
                            "Could not signal proxy pid {} and could not verify it (`ps` failed); leaving the pidfile intact",
                            record.pid
                        )));
                    }
                }
            }
        }
        Some(false) => {
            // Dead, or the PID was reused by an unrelated process — either
            // way the record is stale and signalling would be wrong.
            std::fs::remove_file(&pid_file)?;
            eprintln!(
                "  {} Pid {} no longer belongs to our proxy; removed the stale pidfile without signalling.",
                style("—").dim(),
                record.pid
            );
        }
        None => {
            return Err(ClaudexError::Message(
                "Could not verify process identity (`ps` failed); leaving the pidfile untouched."
                    .into(),
            ));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PinnedPair;

    fn touch_executable(path: &std::path::Path) {
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, "#!/bin/sh\n").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mut perms = std::fs::metadata(path).unwrap().permissions();
            perms.set_mode(0o755);
            std::fs::set_permissions(path, perms).unwrap();
        }
    }

    #[cfg(unix)]
    #[test]
    fn sidecar_env_allowlist_drops_ambient_secrets() {
        // An ambient secret present in the launching environment must NOT reach
        // the long-lived sidecar, while a benign runtime essential (PATH) must.
        // Spawn a shell under the minimal env and inspect what it actually sees —
        // `get_envs()` only reports explicit overrides, so we check the real
        // child environment instead.
        std::env::set_var("OVM_GITHUB_TOKEN", "super-secret-should-not-leak");

        let mut command = std::process::Command::new("/bin/sh");
        command
            .arg("-c")
            .arg("echo \"TOKEN=[${OVM_GITHUB_TOKEN:-}]\"; echo \"PATHSET=[${PATH:+yes}]\"");
        apply_minimal_sidecar_env(&mut command);
        let output = command.output().expect("run sh under minimal env");
        let stdout = String::from_utf8_lossy(&output.stdout);

        std::env::remove_var("OVM_GITHUB_TOKEN");

        assert!(
            !stdout.contains("super-secret-should-not-leak"),
            "ambient OVM_GITHUB_TOKEN leaked into the sidecar env: {stdout}"
        );
        assert!(
            stdout.contains("TOKEN=[]"),
            "expected empty token, got: {stdout}"
        );
        assert!(
            stdout.contains("PATHSET=[yes]"),
            "PATH must be forwarded so the sidecar can run: {stdout}"
        );
    }

    #[test]
    fn resolve_prefers_pinned_version_over_current_symlink() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        let pinned = dirs.proxy_versions_dir().join("7.1.0").join("cliproxyapi");
        let newer = dirs.proxy_versions_dir().join("7.2.0").join("cliproxyapi");
        touch_executable(&pinned);
        touch_executable(&newer);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&newer, dirs.proxy_current()).unwrap();

        let config = ClaudexConfig {
            pin: Some(PinnedPair {
                claude: "2.1.207".into(),
                proxy: "7.1.0".into(),
            }),
            ..ClaudexConfig::default()
        };

        match resolve_binary(&dirs, &config) {
            Some(ProxyBinary::Managed { version, .. }) => assert_eq!(version, "7.1.0"),
            other => panic!(
                "expected pinned managed binary, got {:?}",
                other.map(|b| b.version_label())
            ),
        }
    }

    #[test]
    #[cfg(unix)]
    fn resolve_reads_version_from_current_symlink_target() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        let binary = dirs.proxy_versions_dir().join("7.2.55").join("cliproxyapi");
        touch_executable(&binary);
        std::os::unix::fs::symlink(&binary, dirs.proxy_current()).unwrap();

        match resolve_binary(&dirs, &ClaudexConfig::default()) {
            Some(ProxyBinary::Managed { version, .. }) => assert_eq!(version, "7.2.55"),
            other => panic!(
                "expected managed binary, got {:?}",
                other.map(|b| b.version_label())
            ),
        }
    }

    #[test]
    fn missing_pinned_version_never_falls_back() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        // A perfectly good current symlink exists…
        let newer = dirs.proxy_versions_dir().join("7.2.0").join("cliproxyapi");
        touch_executable(&newer);
        #[cfg(unix)]
        std::os::unix::fs::symlink(&newer, dirs.proxy_current()).unwrap();

        // …but the pin names a version that isn't installed.
        let config = ClaudexConfig {
            pin: Some(PinnedPair {
                claude: "2.1.207".into(),
                proxy: "9.9.9".into(),
            }),
            ..ClaudexConfig::default()
        };
        assert!(
            resolve_binary(&dirs, &config).is_none(),
            "a pin must never silently fall back to an unpinned proxy"
        );
    }

    #[test]
    fn pid_record_round_trips_and_reads_legacy_format() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");

        write_pid_record(
            &dirs,
            4242,
            std::path::Path::new("/x/bin/cliproxyapi"),
            true,
        )
        .expect("write");
        let record = read_pid_record(&dirs.proxy_pid_file()).expect("read");
        assert_eq!(record.pid, 4242);
        assert_eq!(record.binary, PathBuf::from("/x/bin/cliproxyapi"));
        assert!(record.session_guarded);

        // Legacy bare-pid files (pre-identity) still parse.
        std::fs::write(dirs.proxy_pid_file(), "1337\n").unwrap();
        let legacy = read_pid_record(&dirs.proxy_pid_file()).expect("legacy");
        assert_eq!(legacy.pid, 1337);
        assert!(!legacy.session_guarded);
    }

    #[test]
    fn session_lock_downgrades_atomically_to_a_shared_lease() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");

        let exclusive = SessionGuard::acquire(&dirs).expect("exclusive");
        assert!(exclusive.is_exclusive());
        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dirs.proxy_sessions_lock())
            .unwrap();
        assert!(matches!(
            FileExt::try_lock(&contender),
            Err(TryLockError::WouldBlock)
        ));

        let shared = exclusive.downgrade().expect("downgrade");
        assert!(!shared.is_exclusive());
        FileExt::try_lock_shared(&contender).expect("second shared lease");
        let writer = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dirs.proxy_sessions_lock())
            .unwrap();
        assert!(matches!(
            FileExt::try_lock(&writer),
            Err(TryLockError::WouldBlock)
        ));
    }

    #[test]
    #[cfg(unix)]
    fn session_lock_survives_exec_in_the_launched_process() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");
        let shared = SessionGuard::acquire(&dirs)
            .expect("exclusive")
            .downgrade()
            .expect("shared");
        shared.make_inheritable().expect("inheritable");
        let mut child = Command::new("sleep").arg("0.2").spawn().unwrap();
        drop(shared);

        let contender = OpenOptions::new()
            .read(true)
            .write(true)
            .open(dirs.proxy_sessions_lock())
            .unwrap();
        assert!(matches!(
            FileExt::try_lock(&contender),
            Err(TryLockError::WouldBlock)
        ));
        assert!(child.wait().unwrap().success());
        FileExt::try_lock(&contender).expect("child exit releases inherited lease");
    }

    #[test]
    #[cfg(unix)]
    fn failed_stop_keeps_verified_pidfile_for_retry() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");
        let mut child = Command::new("/bin/sh")
            .args(["-c", "trap '' TERM; while :; do sleep 1; done"])
            .spawn()
            .unwrap();
        std::thread::sleep(Duration::from_millis(100));
        write_pid_record(&dirs, child.id(), std::path::Path::new("/bin/sh"), true).unwrap();

        let error = stop_with_attempts(&dirs, 1).expect_err("proxy ignores SIGTERM");
        assert!(error.to_string().contains("did not exit"), "{error}");
        assert!(
            dirs.proxy_pid_file().is_file(),
            "retry identity must survive a stop timeout"
        );

        let _ = Command::new("kill")
            .args(["-KILL", &child.id().to_string()])
            .status();
        let _ = child.wait();
    }

    #[test]
    fn pid_identity_check_rejects_reused_pids() {
        // Our own test process exists but is not cliproxyapi.
        let record = PidRecord {
            pid: std::process::id(),
            binary: PathBuf::from("/x/bin/cliproxyapi"),
            started: 0,
            session_guarded: false,
        };
        assert_eq!(pid_matches_record(&record), Some(false));

        // A PID that can't exist reports "not ours" (dead), never a signal.
        let dead = PidRecord {
            pid: 4_000_000,
            binary: PathBuf::from("/x/bin/cliproxyapi"),
            started: 0,
            session_guarded: false,
        };
        assert_eq!(pid_matches_record(&dead), Some(false));
    }

    #[test]
    fn start_time_mismatch_defeats_pid_reuse() {
        // Name matches (our own test binary), but the recorded start time is
        // ancient — a reused PID must fail identity.
        let exe = std::env::current_exe().unwrap();
        let record = PidRecord {
            pid: std::process::id(),
            binary: exe.clone(),
            started: 1, // 1970 — nothing running now started then
            session_guarded: false,
        };
        assert_eq!(pid_matches_record(&record), Some(false));

        // With a fresh start time it passes (this process just started).
        let record = PidRecord {
            pid: std::process::id(),
            binary: exe,
            started: now_unix(),
            session_guarded: false,
        };
        assert_eq!(pid_matches_record(&record), Some(true));
    }

    #[test]
    fn zombie_process_reads_as_exited() {
        // A killed-but-unreaped child still answers `ps` on Linux with its
        // plain comm and a live etime; identity must report it as exited or
        // stop-wait loops spin for the full timeout on a corpse.
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let record = PidRecord {
            pid: child.id(),
            binary: PathBuf::from("/bin/sleep"),
            started: now_unix(),
            session_guarded: false,
        };
        assert_eq!(pid_matches_record(&record), Some(true));

        child.kill().unwrap();
        // Deliberately not reaped yet: the child is now a zombie.
        std::thread::sleep(std::time::Duration::from_millis(200));
        assert_eq!(pid_matches_record(&record), Some(false));
        let _ = child.wait();
    }

    #[test]
    fn etime_parsing_handles_all_ps_formats() {
        assert_eq!(parse_etime("00:42"), Some(42));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3723));
        assert_eq!(parse_etime("garbage"), None);
    }

    #[test]
    fn nothing_resolves_in_an_empty_layout_without_path_fallback() {
        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        // PATH may contain a real cliproxyapi on dev machines; only assert the
        // managed layers stay empty by checking the resolved source kind.
        if let Some(binary) = resolve_binary(&dirs, &ClaudexConfig::default()) {
            assert!(
                matches!(binary, ProxyBinary::System { .. }),
                "empty layout must never resolve a managed binary"
            );
        }
    }

    /// Fake HTTP listener that answers every connection: 200 + model list
    /// when the request carries `Bearer <expected_key>`, 401 otherwise —
    /// i.e. it behaves like a real CLIProxyAPI. The seed of the P5 fake proxy.
    fn fake_keyed_listener(expected_key: &'static str) -> u16 {
        serve(move |request| {
            if request.contains(&format!("Bearer {expected_key}")) {
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"data\":[{\"id\":\"gpt-5.6-sol\"}]}".to_string()
            } else {
                "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}".to_string()
            }
        })
    }

    /// Fake listener with a fixed response for every request.
    fn fake_listener(response: &'static str) -> u16 {
        serve(move |_| response.to_string())
    }

    fn serve(respond: impl Fn(&str) -> String + Send + 'static) -> u16 {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let mut stream = stream;
                use std::io::{Read, Write};
                let mut buffer = [0u8; 4096];
                let read = stream.read(&mut buffer).unwrap_or(0);
                let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
                let _ = stream.write_all(respond(&request).as_bytes());
            }
        });
        port
    }

    #[test]
    fn probe_verifies_a_key_checking_proxy() {
        let port = fake_keyed_listener("local-key");
        assert_eq!(
            probe(port, "local-key", ProbeIdentity::TrustForTest),
            ProxyProbe::Verified
        );
    }

    #[test]
    fn probe_flags_a_listener_that_accepts_any_key() {
        // A squatter that 200s everything gets caught by the canary request
        // BEFORE the real key is ever sent.
        let port = fake_listener(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"data\":[]}",
        );
        match probe(port, "local-key", ProbeIdentity::TrustForTest) {
            ProxyProbe::ForeignListener(why) => {
                assert!(why.contains("accepted an invalid key"), "{why}")
            }
            other => panic!("expected foreign listener, got {other:?}"),
        }
    }

    #[test]
    fn probe_flags_a_listener_that_rejects_our_key() {
        let port = fake_listener("HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}");
        match probe(port, "local-key", ProbeIdentity::TrustForTest) {
            ProxyProbe::ForeignListener(why) => assert!(why.starts_with("HTTP 401"), "{why}"),
            other => panic!("expected foreign listener, got {other:?}"),
        }
    }

    #[test]
    fn probe_flags_a_non_http_squatter() {
        let port = fake_listener("i am not a proxy\r\n\r\n");
        assert!(matches!(
            probe(port, "local-key", ProbeIdentity::TrustForTest),
            ProxyProbe::ForeignListener(_)
        ));
    }

    #[test]
    fn probe_reports_a_free_port_as_down() {
        // Port 1 is never bindable by user processes, so the connection is
        // reliably refused. (A bind-then-drop ephemeral port is flaky here:
        // parallel tests recycle freed ports immediately.)
        assert_eq!(
            probe(1, "local-key", ProbeIdentity::TrustForTest),
            ProxyProbe::Down
        );
    }

    /// Regression for the credential-disclosure finding: a listener that
    /// rejects the canary (a genuine-looking 401) but that claudex cannot tie
    /// to a verified proxy pidfile must NEVER receive the configured key.
    #[test]
    fn probe_never_sends_the_key_to_an_unverified_listener() {
        use std::sync::{Arc, Mutex};

        let seen: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let recorder = Arc::clone(&seen);
        // 401s everything and records every request it receives.
        let port = serve(move |request| {
            recorder.lock().unwrap().push(request.to_string());
            "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}".to_string()
        });

        let temp = tempfile::tempdir().expect("tempdir");
        let dirs = ClaudexDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().expect("layout");

        // No pidfile exists, so identity cannot be confirmed and the key must
        // be withheld.
        match probe(port, "secret-configured-key", ProbeIdentity::Pidfile(&dirs)) {
            ProxyProbe::Unverified(_) => {}
            other => panic!("expected Unverified, got {other:?}"),
        }

        let requests = seen.lock().unwrap();
        assert!(
            requests
                .iter()
                .all(|request| !request.contains("secret-configured-key")),
            "the configured key was transmitted to an unverified listener: {requests:?}"
        );
        assert!(
            requests
                .iter()
                .any(|request| request.contains("Bearer canary-")),
            "the canary probe (random key) should still have been attempted"
        );
    }
}
