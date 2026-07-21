//! End-to-end tests across the integration boundary: the real `ovm-claudex`
//! binary against a fake proxy (a tiny HTTP listener) and a fake `ovm`
//! (a script that captures the args and environment it would have exec'd).
//! No network, no OAuth, no real Claude.

use assert_cmd::cargo::CommandCargoExt;
use assert_cmd::Command;
use sha2::Digest;
use std::io::{Read, Write};
use std::net::TcpListener;
use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

static PROCESS_TEST_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn serialize_process_test() -> std::sync::MutexGuard<'static, ()> {
    PROCESS_TEST_LOCK
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Serve HTTP forever on an ephemeral port, behaving like a real
/// CLIProxyAPI: 200 + model list for `Bearer e2e-key`, 401 for anything
/// else (the launcher's two-step probe requires exactly this shape).
fn fake_proxy_keyed() -> u16 {
    serve(|request| {
        if request.contains("Bearer e2e-key") {
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nConnection: close\r\n\r\n{\"data\":[{\"id\":\"gpt-5.6-sol\"}]}".to_string()
        } else {
            "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}".to_string()
        }
    })
}

fn serve(respond: impl Fn(&str) -> String + Send + 'static) -> u16 {
    serve_bytes(move |request| respond(request).into_bytes())
}

fn serve_bytes(respond: impl Fn(&str) -> Vec<u8> + Send + 'static) -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    let port = listener.local_addr().expect("addr").port();
    std::thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let mut stream = stream;
            let mut buffer = [0u8; 4096];
            let read = stream.read(&mut buffer).unwrap_or(0);
            let request = String::from_utf8_lossy(&buffer[..read]).into_owned();
            let _ = stream.write_all(&respond(&request));
        }
    });
    port
}

fn http_response(content_type: &str, body: &[u8]) -> Vec<u8> {
    let mut response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    )
    .into_bytes();
    response.extend_from_slice(body);
    response
}

fn release_asset(version: &str) -> String {
    let os = if cfg!(target_os = "macos") {
        "darwin"
    } else {
        "linux"
    };
    let arch = if cfg!(target_arch = "aarch64") {
        "aarch64"
    } else {
        "amd64"
    };
    format!("CLIProxyAPI_{version}_{os}_{arch}.tar.gz")
}

fn proxy_tarball(contents: &[u8]) -> Vec<u8> {
    let mut builder = tar::Builder::new(Vec::new());
    let mut header = tar::Header::new_gnu();
    header.set_size(contents.len() as u64);
    header.set_mode(0o755);
    header.set_cksum();
    builder
        .append_data(&mut header, "cli-proxy-api", contents)
        .unwrap();
    let tarball = builder.into_inner().unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::fast());
    encoder.write_all(&tarball).unwrap();
    encoder.finish().unwrap()
}

struct ReleaseServer {
    base_url: String,
    latest_requests: Arc<AtomicUsize>,
    asset_requests: Arc<AtomicUsize>,
    checksum_requests: Arc<AtomicUsize>,
}

fn fake_release_server(version: &'static str, archive: Vec<u8>) -> ReleaseServer {
    let asset = release_asset(version);
    let checksum: String = sha2::Sha256::digest(&archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let latest_requests = Arc::new(AtomicUsize::new(0));
    let asset_requests = Arc::new(AtomicUsize::new(0));
    let checksum_requests = Arc::new(AtomicUsize::new(0));
    let counters = (
        Arc::clone(&latest_requests),
        Arc::clone(&asset_requests),
        Arc::clone(&checksum_requests),
    );
    let asset_for_server = asset.clone();
    let port = serve_bytes(move |request| {
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if path.ends_with("/releases/latest") {
            counters.0.fetch_add(1, Ordering::SeqCst);
            return http_response(
                "application/json",
                format!(r#"{{"tag_name":"v{version}"}}"#).as_bytes(),
            );
        }
        if path.ends_with(&asset_for_server) {
            counters.1.fetch_add(1, Ordering::SeqCst);
            return http_response("application/gzip", &archive);
        }
        if path.ends_with("checksums.txt") {
            counters.2.fetch_add(1, Ordering::SeqCst);
            return http_response(
                "text/plain",
                format!("{checksum}  {asset_for_server}\n").as_bytes(),
            );
        }
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
    });
    ReleaseServer {
        base_url: format!("http://127.0.0.1:{port}"),
        latest_requests,
        asset_requests,
        checksum_requests,
    }
}

fn unused_local_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind");
    listener.local_addr().expect("addr").port()
}

fn compile_fake_proxy(path: &Path) {
    let source = path.with_extension("c");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(
        &source,
        r#"
#include <arpa/inet.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

// Real cliproxyapi reads its port and API key from the `--config` file, and so
// does this fake — the claudex sidecar is spawned with a minimal, allowlisted
// environment (no OVM_FAKE_* vars leak into it), so config-file parsing is the
// only faithful way to learn them.
static int find_config(int argc, char **argv, char *out, int cap) {
    for (int i = 1; i + 1 < argc; i++) {
        if (strcmp(argv[i], "--config") == 0) {
            snprintf(out, cap, "%s", argv[i + 1]);
            return 1;
        }
    }
    return 0;
}

int main(int argc, char **argv) {
    char config_path[4096] = {0};
    if (!find_config(argc, argv, config_path, sizeof(config_path))) return 3;
    FILE *cf = fopen(config_path, "rb");
    if (!cf) return 4;
    static char text[65536];
    size_t n = fread(text, 1, sizeof(text) - 1, cf);
    text[n] = 0;
    fclose(cf);

    char *pp = strstr(text, "port:");
    int port = pp ? atoi(pp + 5) : 0;

    // Skip the "api-keys:" label (it contains a hyphen) before hunting the
    // list entry's dash, then read the (optionally quoted) key value.
    char key[512] = {0};
    char *ak = strstr(text, "api-keys:");
    if (ak) {
        char *dash = strchr(ak + 9, '-');
        if (dash) {
            char *v = dash + 1;
            while (*v == ' ' || *v == '\t' || *v == '"') v++;
            int j = 0;
            while (*v && *v != '"' && *v != '\n' && *v != '\r' && j < (int)sizeof(key) - 1) {
                key[j++] = *v++;
            }
            key[j] = 0;
        }
    }
    if (port == 0 || key[0] == 0) return 5;

    int server = socket(AF_INET, SOCK_STREAM, 0);
    int yes = 1;
    setsockopt(server, SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes));
    struct sockaddr_in addr = {0};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(port);
    if (bind(server, (struct sockaddr *)&addr, sizeof(addr)) != 0 || listen(server, 16) != 0) return 2;
    char expected[512];
    snprintf(expected, sizeof(expected), "Authorization: Bearer %s", key);
    for (;;) {
        int client = accept(server, NULL, NULL);
        if (client < 0) continue;
        char request[8192] = {0};
        read(client, request, sizeof(request) - 1);
        if (strstr(request, expected)) {
            const char *ok = "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\nConnection: close\r\n\r\n{\"data\":[]}";
            write(client, ok, strlen(ok));
        } else {
            const char *no = "HTTP/1.1 401 Unauthorized\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}";
            write(client, no, strlen(no));
        }
        close(client);
    }
}
"#,
    )
    .unwrap();
    let status = std::process::Command::new("cc")
        .arg(&source)
        .args(["-O2", "-o"])
        .arg(path)
        .status()
        .unwrap();
    assert!(status.success(), "failed to compile fake proxy");
}

/// A claudex home with config + seeded claude dir, pointed at `port`.
fn seeded_home(temp: &Path, port: u16) {
    let claude_home = temp.join("claude");
    std::fs::create_dir_all(&claude_home).unwrap();
    std::fs::write(claude_home.join(".claude.json"), "{}").unwrap();
    std::fs::write(
        temp.join("config.json"),
        format!(r#"{{"proxy": {{"port": {port}, "api_key": "e2e-key"}}}}"#),
    )
    .unwrap();
}

/// Simulate an already-running proxy daemon that claudex adopts. The in-thread
/// `fake_proxy_*` listeners are owned by *this* test process, so the pidfile
/// records this process — its pid owns the LISTEN socket, so claudex's identity
/// probe (pidfile + `ps` + `lsof` socket ownership) confirms it, exactly as it
/// would a real prior-session daemon. `session_guarded` selects the modern
/// (true) vs. legacy pre-session-tracking (false) daemon shape.
fn adopt_running_proxy(temp: &Path, session_guarded: bool) {
    let proxy_dir = temp.join("proxy");
    std::fs::create_dir_all(&proxy_dir).unwrap();
    let exe = std::env::current_exe().unwrap();
    std::fs::write(
        proxy_dir.join("cliproxyapi.pid"),
        serde_json::to_vec(&serde_json::json!({
            "pid": std::process::id(),
            "binary": exe,
            // The test binary is long-lived; recording a real start time would
            // fail the 90s etime-drift check once the suite has run a while.
            // `started: 0` uses the legacy shape, so identity rests on the
            // command name + `lsof` socket ownership — exactly what this
            // adoption path must prove.
            "started": 0,
            "session_guarded": session_guarded,
        }))
        .unwrap(),
    )
    .unwrap();
}

/// A rejecting listener (401 to every request) that records the raw request
/// lines it received, so a test can prove the launcher never leaked the real
/// proxy key to an unverified listener.
fn fake_proxy_rejecting_recording() -> (u16, Arc<Mutex<Vec<String>>>) {
    let log: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recorder = Arc::clone(&log);
    let port = serve(move |request| {
        recorder.lock().unwrap().push(request.to_string());
        "HTTP/1.1 401 Unauthorized\r\nConnection: close\r\n\r\n{}".to_string()
    });
    (port, log)
}

fn set_auto_update(temp: &Path, enabled: bool) {
    let path = temp.join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    config["auto_update_proxy"] = enabled.into();
    std::fs::write(path, serde_json::to_vec(&config).unwrap()).unwrap();
}

fn wait_until(message: &str, ready: impl Fn() -> bool) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while std::time::Instant::now() < deadline {
        if ready() {
            return;
        }
        std::thread::sleep(std::time::Duration::from_millis(25));
    }
    panic!("timed out waiting for {message}");
}

fn proxy_pid(temp: &Path) -> u64 {
    let record: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(temp.join("proxy/cliproxyapi.pid")).unwrap())
            .unwrap();
    record["pid"].as_u64().unwrap()
}

fn now_unix() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// A fake `ovm` on PATH that records its argv and env, then exits 0.
fn fake_ovm(temp: &Path) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let bin = temp.join("bin");
    std::fs::create_dir_all(&bin).unwrap();
    let args_file = temp.join("captured-args");
    let env_file = temp.join("captured-env");
    let script = format!(
        "#!/bin/sh\nprintf '%s\\n' \"$@\" > {}\nenv > {}\nif [ \"${{1:-}}\" != current ] && [ -n \"${{OVM_FAKE_WAIT_FILE:-}}\" ]; then\n  while [ -e \"$OVM_FAKE_WAIT_FILE\" ]; do sleep 0.05; done\nfi\nexit \"${{OVM_FAKE_EXIT:-0}}\"\n",
        args_file.display(),
        env_file.display()
    );
    let ovm = bin.join("ovm");
    std::fs::write(&ovm, script).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&ovm, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    (bin, args_file, env_file)
}

#[test]
fn doctor_fails_clearly_before_setup() {
    let temp = tempfile::tempdir().unwrap();
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .arg("doctor")
        .env("OVM_CLAUDEX_HOME", temp.path())
        .output()
        .unwrap();
    assert!(!output.status.success(), "virgin home must fail doctor");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("ovm claudex setup"), "{stderr}");
}

#[test]
fn session_start_archives_and_reuses_the_feedback_relationship() {
    let temp = tempfile::tempdir().unwrap();
    let env_file = temp.path().join("claude-env");
    let hook_input = r#"{
        "session_id": "claude-history-123",
        "transcript_path": "/tmp/claudex-history.jsonl",
        "source": "startup",
        "model": "gpt-5.6-sol"
    }"#;

    for _ in 0..2 {
        let output = Command::cargo_bin("ovm-claudex")
            .unwrap()
            .arg("__session-start")
            .env("OVM_CLAUDEX_HOME", temp.path())
            .env("CLAUDE_ENV_FILE", &env_file)
            .write_stdin(hook_input)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "hook failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }

    let relationships = temp.path().join("history/relationships");
    let records = std::fs::read_dir(&relationships)
        .unwrap()
        .collect::<std::result::Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(records.len(), 1, "resume must reuse the relationship");

    let record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(records[0].path()).expect("relationship JSON"),
    )
    .unwrap();
    let feedback_id = record["feedback_id"].as_str().unwrap();
    assert!(feedback_id.starts_with("cfx_"));
    assert_eq!(record["claude_session_id"], "claude-history-123");
    assert_eq!(record["codex_association"]["bridge"], "cliproxyapi");
    assert_eq!(
        record["codex_association"]["join_header"],
        "X-Claude-Code-Session-Id"
    );

    let lookup = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .arg("feedback-id")
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("CLAUDE_CODE_SESSION_ID", "claude-history-123")
        .env_remove("CLAUDEX_FEEDBACK_ID")
        .output()
        .unwrap();
    assert!(lookup.status.success());
    assert_eq!(String::from_utf8_lossy(&lookup.stdout).trim(), feedback_id);
}

#[test]
fn launch_wires_model_env_and_scrubs_ambient_credentials() {
    let temp = tempfile::tempdir().unwrap();
    let port = fake_proxy_keyed();
    seeded_home(temp.path(), port);
    adopt_running_proxy(temp.path(), true);
    let (bin, args_file, env_file) = fake_ovm(temp.path());

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        // A live ambient credential MUST NOT reach the child.
        .env("ANTHROPIC_API_KEY", "leaked-secret")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "launch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let args = std::fs::read_to_string(&args_file).unwrap();
    assert_eq!(
        args.lines().collect::<Vec<_>>(),
        vec!["cc", "--model", "gpt-5.6-sol", "-p", "hello"],
        "default model injection + passthrough must reach ovm"
    );

    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(
        env.contains(&format!("ANTHROPIC_BASE_URL=http://127.0.0.1:{port}")),
        "proxy wiring missing:\n{env}"
    );
    assert!(env.contains("ANTHROPIC_AUTH_TOKEN=e2e-key"));
    assert!(env.contains("ANTHROPIC_DEFAULT_OPUS_MODEL=gpt-5.6-sol"));
    assert!(env.contains("CLAUDE_CODE_SUBAGENT_MODEL=gpt-5.6-terra"));
    assert!(
        !env.contains("leaked-secret"),
        "ambient ANTHROPIC_API_KEY leaked into the child:\n{env}"
    );
    let claude_home = temp.path().join("claude");
    assert!(env.contains(&format!("CLAUDE_CONFIG_DIR={}", claude_home.display())));
}

#[test]
#[cfg(unix)]
fn launch_prepares_newer_managed_proxy_once_without_interrupting_legacy_daemon() {
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = fake_proxy_keyed();
    seeded_home(temp.path(), proxy_port);
    // A pre-session-tracking daemon: pidfile present but not session-guarded.
    adopt_running_proxy(temp.path(), false);
    let (bin, _, _) = fake_ovm(temp.path());

    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    std::fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
    std::fs::write(&old_binary, b"old-proxy").unwrap();
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();

    let archive = proxy_tarball(b"new-proxy");
    let releases = fake_release_server("7.2.74", archive);
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    for _ in 0..2 {
        let output = Command::cargo_bin("ovm-claudex")
            .unwrap()
            .args(["-p", "hello"])
            .env("OVM_CLAUDEX_HOME", temp.path())
            .env("PATH", &path_env)
            .env("OVM_GITHUB_API_URL", &releases.base_url)
            .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
            // The mock has no /registry.json (404) — the registry lookup must
            // fail open onto the GitHub releases path this test asserts on.
            .env("OVM_CLAUDEX_REGISTRY_URL", &releases.base_url)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "launch failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let stderr = String::from_utf8_lossy(&output.stderr);
        assert!(
            stderr.contains("predates safe session tracking"),
            "{stderr}"
        );
    }

    let new_binary = temp.path().join("proxy/versions/7.2.74/cliproxyapi");
    assert_eq!(std::fs::read(&new_binary).unwrap(), b"new-proxy");
    assert_eq!(
        std::fs::canonicalize(temp.path().join("proxy/current")).unwrap(),
        std::fs::canonicalize(&old_binary).unwrap(),
        "a pre-session-lock daemon must not be restarted underneath users"
    );
    let pending: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("proxy/pending-update.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pending["version"], "7.2.74");
    assert_eq!(releases.latest_requests.load(Ordering::SeqCst), 1);
    assert_eq!(releases.asset_requests.load(Ordering::SeqCst), 1);
    assert_eq!(releases.checksum_requests.load(Ordering::SeqCst), 1);
}

#[test]
#[cfg(unix)]
fn launch_prepares_the_registry_pinned_version_over_a_newer_github_release() {
    // The OVM registry vouches only for deep-lane-verified builds, so when it
    // pins an OLDER version than GitHub's releases/latest, claudex must target
    // the registry version and never consult releases/latest at all.
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = fake_proxy_keyed();
    seeded_home(temp.path(), proxy_port);
    // A pre-session-tracking daemon: staged updates must not restart it.
    adopt_running_proxy(temp.path(), false);
    let (bin, _, _) = fake_ovm(temp.path());

    let old_binary = temp.path().join("proxy/versions/7.2.70/cliproxyapi");
    std::fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
    std::fs::write(&old_binary, b"old-proxy").unwrap();
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();

    // Registry pins 7.2.72 (verified); GitHub's releases/latest advertises the
    // newer, unverified 7.2.74. Only the 7.2.72 asset is served.
    let registry_version = "7.2.72";
    let asset = release_asset(registry_version);
    let archive = proxy_tarball(b"registry-proxy");
    let checksum: String = sha2::Sha256::digest(&archive)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect();
    let releases_latest_hits = Arc::new(AtomicUsize::new(0));
    let registry_hits = Arc::new(AtomicUsize::new(0));
    let counters = (
        Arc::clone(&releases_latest_hits),
        Arc::clone(&registry_hits),
    );
    let asset_for_server = asset.clone();
    let port = serve_bytes(move |request| {
        let path = request
            .lines()
            .next()
            .and_then(|line| line.split_whitespace().nth(1))
            .unwrap_or("");
        if path.ends_with("/registry.json") {
            counters.1.fetch_add(1, Ordering::SeqCst);
            return http_response(
                "application/json",
                br#"{"products":[{"product":"claude","latest":"2.1.212"},{"product":"cliproxyapi","latest":"7.2.72"}]}"#,
            );
        }
        if path.ends_with("/releases/latest") {
            counters.0.fetch_add(1, Ordering::SeqCst);
            return http_response("application/json", br#"{"tag_name":"v7.2.74"}"#);
        }
        if path.ends_with(&asset_for_server) {
            return http_response("application/gzip", &archive);
        }
        if path.ends_with("checksums.txt") {
            return http_response(
                "text/plain",
                format!("{checksum}  {asset_for_server}\n").as_bytes(),
            );
        }
        b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_vec()
    });
    let base_url = format!("http://127.0.0.1:{port}");
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", &path_env)
        .env("OVM_CLAUDEX_REGISTRY_URL", &base_url)
        .env("OVM_GITHUB_API_URL", &base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &base_url)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "launch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let new_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    assert_eq!(std::fs::read(&new_binary).unwrap(), b"registry-proxy");
    let pending: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("proxy/pending-update.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(pending["version"], "7.2.72");
    assert_eq!(
        registry_hits.load(Ordering::SeqCst),
        1,
        "the registry must be consulted"
    );
    assert_eq!(
        releases_latest_hits.load(Ordering::SeqCst),
        0,
        "releases/latest must not be consulted once the registry answers"
    );
}

#[test]
#[cfg(unix)]
fn next_safe_launch_activates_and_verifies_prepared_proxy() {
    let _serial = serialize_process_test();
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = unused_local_port();
    seeded_home(temp.path(), proxy_port);
    std::fs::create_dir_all(temp.path().join("proxy")).unwrap();
    std::fs::write(
        temp.path().join("proxy/config.yaml"),
        format!("port: {proxy_port}\napi-keys:\n  - \"e2e-key\"\n"),
    )
    .unwrap();
    let (bin, _, _) = fake_ovm(temp.path());

    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    std::fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
    std::fs::write(&old_binary, b"old-proxy").unwrap();
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();

    let compiled_proxy = temp.path().join("fixture/cliproxyapi");
    compile_fake_proxy(&compiled_proxy);
    let releases = fake_release_server(
        "7.2.74",
        proxy_tarball(&std::fs::read(compiled_proxy).unwrap()),
    );
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_GITHUB_API_URL", &releases.base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .env("OVM_CLAUDEX_REGISTRY_URL", &releases.base_url)
        .output()
        .unwrap();

    assert!(
        output.status.success(),
        "launch failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("Activated and verified cliproxyapi 7.2.74"),
        "{stderr}"
    );
    let new_binary = temp.path().join("proxy/versions/7.2.74/cliproxyapi");
    assert_eq!(
        std::fs::canonicalize(temp.path().join("proxy/current")).unwrap(),
        std::fs::canonicalize(&new_binary).unwrap()
    );
    assert!(!temp.path().join("proxy/pending-update.json").exists());
    let pid_record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("proxy/cliproxyapi.pid")).unwrap(),
    )
    .unwrap();
    assert_eq!(pid_record["session_guarded"], true);

    let pid = pid_record["pid"].as_u64().unwrap().to_string();
    let _ = std::process::Command::new("kill").arg(pid).status();
}

#[test]
#[cfg(unix)]
fn failed_proxy_activation_rolls_back_current_and_restarts_previous_binary() {
    let _serial = serialize_process_test();
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = unused_local_port();
    seeded_home(temp.path(), proxy_port);
    std::fs::create_dir_all(temp.path().join("proxy")).unwrap();
    std::fs::write(
        temp.path().join("proxy/config.yaml"),
        format!("port: {proxy_port}\napi-keys:\n  - \"e2e-key\"\n"),
    )
    .unwrap();
    let (bin, _, _) = fake_ovm(temp.path());
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    compile_fake_proxy(&old_binary);
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();
    let releases = fake_release_server("7.2.74", proxy_tarball(b"#!/bin/sh\nexit 1\n"));

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_GITHUB_API_URL", &releases.base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .env("OVM_CLAUDEX_REGISTRY_URL", &releases.base_url)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "launch should fail open on the restored proxy: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("rolled `current` back"), "{stderr}");
    assert_eq!(
        std::fs::canonicalize(temp.path().join("proxy/current")).unwrap(),
        std::fs::canonicalize(&old_binary).unwrap()
    );
    let pid_record: serde_json::Value = serde_json::from_str(
        &std::fs::read_to_string(temp.path().join("proxy/cliproxyapi.pid")).unwrap(),
    )
    .unwrap();
    assert_eq!(
        std::fs::canonicalize(pid_record["binary"].as_str().unwrap()).unwrap(),
        std::fs::canonicalize(&old_binary).unwrap()
    );
    assert!(temp.path().join("proxy/pending-update.json").is_file());

    let _ = std::process::Command::new("kill")
        .arg(pid_record["pid"].as_u64().unwrap().to_string())
        .status();
}

#[test]
#[cfg(unix)]
fn overlapping_session_stages_once_then_next_idle_launch_activates() {
    let _serial = serialize_process_test();
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = unused_local_port();
    seeded_home(temp.path(), proxy_port);
    set_auto_update(temp.path(), false);
    std::fs::create_dir_all(temp.path().join("proxy")).unwrap();
    std::fs::write(
        temp.path().join("proxy/config.yaml"),
        format!("port: {proxy_port}\napi-keys:\n  - \"e2e-key\"\n"),
    )
    .unwrap();
    let (bin, args_file, _) = fake_ovm(temp.path());
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    compile_fake_proxy(&old_binary);
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();
    let releases = fake_release_server(
        "7.2.74",
        proxy_tarball(&std::fs::read(&old_binary).unwrap()),
    );

    let wait_file = temp.path().join("keep-first-session-open");
    std::fs::write(&wait_file, "open").unwrap();
    let mut first_command = std::process::Command::cargo_bin("ovm-claudex").unwrap();
    first_command
        .args(["-p", "first"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", &path_env)
        .env("OVM_FAKE_WAIT_FILE", &wait_file)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    let mut first = first_command.spawn().unwrap();
    wait_until("first claudex session", || {
        std::fs::read_to_string(&args_file)
            .map(|args| args.lines().next() == Some("cc"))
            .unwrap_or(false)
            && temp.path().join("proxy/cliproxyapi.pid").is_file()
    });
    let old_pid = proxy_pid(temp.path());

    set_auto_update(temp.path(), true);
    let second = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "second"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", &path_env)
        .env("OVM_GITHUB_API_URL", &releases.base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .env("OVM_CLAUDEX_REGISTRY_URL", &releases.base_url)
        .output()
        .unwrap();
    assert!(
        second.status.success(),
        "{}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(
        proxy_pid(temp.path()),
        old_pid,
        "live session was interrupted"
    );
    assert!(temp.path().join("proxy/pending-update.json").is_file());
    assert_eq!(
        std::fs::canonicalize(temp.path().join("proxy/current")).unwrap(),
        std::fs::canonicalize(&old_binary).unwrap()
    );

    std::fs::remove_file(wait_file).unwrap();
    assert!(first.wait().unwrap().success());

    let third = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "third"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_GITHUB_API_URL", &releases.base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .env("OVM_CLAUDEX_REGISTRY_URL", &releases.base_url)
        .output()
        .unwrap();
    assert!(
        third.status.success(),
        "{}",
        String::from_utf8_lossy(&third.stderr)
    );
    assert!(String::from_utf8_lossy(&third.stderr)
        .contains("Activated and verified cliproxyapi 7.2.74"));
    assert_ne!(proxy_pid(temp.path()), old_pid);
    assert!(!temp.path().join("proxy/pending-update.json").exists());

    let _ = std::process::Command::new("kill")
        .arg(proxy_pid(temp.path()).to_string())
        .status();
}

#[test]
#[cfg(unix)]
fn explicit_update_migrates_a_legacy_daemon_after_sessions_are_closed() {
    let _serial = serialize_process_test();
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = unused_local_port();
    seeded_home(temp.path(), proxy_port);
    std::fs::create_dir_all(temp.path().join("proxy")).unwrap();
    std::fs::write(
        temp.path().join("proxy/config.yaml"),
        format!("port: {proxy_port}\napi-keys:\n  - \"e2e-key\"\n"),
    )
    .unwrap();
    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    compile_fake_proxy(&old_binary);
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();

    let mut old_proxy = std::process::Command::new(&old_binary)
        .arg("--config")
        .arg(temp.path().join("proxy/config.yaml"))
        .spawn()
        .unwrap();
    wait_until("legacy proxy", || {
        std::net::TcpStream::connect(("127.0.0.1", proxy_port)).is_ok()
    });
    std::fs::write(
        temp.path().join("proxy/cliproxyapi.pid"),
        serde_json::to_vec(&serde_json::json!({
            "pid": old_proxy.id(),
            "binary": old_binary,
            "started": now_unix(),
            "session_guarded": false
        }))
        .unwrap(),
    )
    .unwrap();

    let compiled_new = temp.path().join("fixture/cliproxyapi");
    compile_fake_proxy(&compiled_new);
    let releases = fake_release_server(
        "7.2.74",
        proxy_tarball(&std::fs::read(compiled_new).unwrap()),
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["update", "7.2.74"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stderr)
        .contains("Activated and verified cliproxyapi 7.2.74"));
    assert_ne!(proxy_pid(temp.path()), old_proxy.id() as u64);
    assert_eq!(
        std::fs::canonicalize(temp.path().join("proxy/current")).unwrap(),
        std::fs::canonicalize(temp.path().join("proxy/versions/7.2.74/cliproxyapi")).unwrap()
    );

    let _ = old_proxy.wait();
    let _ = std::process::Command::new("kill")
        .arg(proxy_pid(temp.path()).to_string())
        .status();
}

#[test]
#[cfg(unix)]
fn launch_honors_disabled_proxy_auto_update_policy() {
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = fake_proxy_keyed();
    seeded_home(temp.path(), proxy_port);
    adopt_running_proxy(temp.path(), true);
    let config_path = temp.path().join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["auto_update_proxy"] = false.into();
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let (bin, _, _) = fake_ovm(temp.path());

    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    std::fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
    std::fs::write(&old_binary, b"old-proxy").unwrap();
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();

    let releases = fake_release_server("7.2.74", proxy_tarball(b"new-proxy"));
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_GITHUB_API_URL", &releases.base_url)
        .env("OVM_CLAUDEX_DOWNLOAD_URL", &releases.base_url)
        .output()
        .unwrap();

    assert!(output.status.success());
    assert_eq!(releases.latest_requests.load(Ordering::SeqCst), 0);
    assert_eq!(releases.asset_requests.load(Ordering::SeqCst), 0);
    assert!(!temp.path().join("proxy/pending-update.json").exists());
}

#[test]
#[cfg(unix)]
fn launch_fails_open_when_release_lookup_is_offline() {
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = fake_proxy_keyed();
    seeded_home(temp.path(), proxy_port);
    adopt_running_proxy(temp.path(), true);
    let (bin, _, _) = fake_ovm(temp.path());
    let old_binary = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    std::fs::create_dir_all(old_binary.parent().unwrap()).unwrap();
    std::fs::write(&old_binary, b"proxy").unwrap();
    std::os::unix::fs::symlink(&old_binary, temp.path().join("proxy/current")).unwrap();
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_GITHUB_API_URL", "http://127.0.0.1:1")
        // Point the registry at the same dead port so the lookup fails open
        // to the (also offline) GitHub path instead of reaching real ovm.sh.
        .env("OVM_CLAUDEX_REGISTRY_URL", "http://127.0.0.1:1")
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("continuing with the installed proxy"),
        "{stderr}"
    );
}

#[test]
#[cfg(unix)]
fn launch_refuses_verified_proxy_that_cannot_be_proven_to_match_pin() {
    let temp = tempfile::tempdir().unwrap();
    let proxy_port = fake_proxy_keyed();
    seeded_home(temp.path(), proxy_port);
    adopt_running_proxy(temp.path(), true);
    let config_path = temp.path().join("config.json");
    let mut config: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&config_path).unwrap()).unwrap();
    config["pin"] = serde_json::json!({"claude": "2.1.209", "proxy": "7.2.72"});
    std::fs::write(&config_path, serde_json::to_vec(&config).unwrap()).unwrap();
    let (bin, args_file, _) = fake_ovm(temp.path());

    let pinned = temp.path().join("proxy/versions/7.2.72/cliproxyapi");
    std::fs::create_dir_all(pinned.parent().unwrap()).unwrap();
    std::fs::write(&pinned, b"proxy").unwrap();
    std::os::unix::fs::symlink(&pinned, temp.path().join("proxy/current")).unwrap();
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .output()
        .unwrap();
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("does not match pinned version 7.2.72"),
        "{stderr}"
    );
    assert!(
        !args_file.exists(),
        "Claude must not launch against a mismatched pin"
    );
}

#[test]
fn launch_fast_selects_fast_aliases() {
    let temp = tempfile::tempdir().unwrap();
    let port = fake_proxy_keyed();
    seeded_home(temp.path(), port);
    adopt_running_proxy(temp.path(), true);
    let (bin, args_file, env_file) = fake_ovm(temp.path());

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["--fast", "-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .output()
        .unwrap();
    assert!(output.status.success());

    let args = std::fs::read_to_string(&args_file).unwrap();
    assert!(args.contains("gpt-5.6-sol-fast"), "{args}");
    let env = std::fs::read_to_string(&env_file).unwrap();
    assert!(env.contains("ANTHROPIC_DEFAULT_HAIKU_MODEL=gpt-5.6-luna-fast"));
}

#[test]
fn launch_exec_propagates_the_claude_exit_code() {
    let temp = tempfile::tempdir().unwrap();
    let port = fake_proxy_keyed();
    seeded_home(temp.path(), port);
    adopt_running_proxy(temp.path(), true);
    let (bin, _, _) = fake_ovm(temp.path());
    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .env("OVM_FAKE_EXIT", "7")
        .output()
        .unwrap();

    assert_eq!(output.status.code(), Some(7));
}

#[test]
fn launch_refuses_a_listener_that_is_not_our_proxy() {
    let temp = tempfile::tempdir().unwrap();
    // A rejecting listener with no pidfile: claudex cannot confirm it owns the
    // socket, so it must refuse WITHOUT ever sending the configured key.
    let (port, request_log) = fake_proxy_rejecting_recording();
    seeded_home(temp.path(), port);
    let (bin, args_file, _) = fake_ovm(temp.path());

    let path_env = format!(
        "{}:{}",
        bin.display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .args(["-p", "hello"])
        .env("OVM_CLAUDEX_HOME", temp.path())
        .env("PATH", path_env)
        .output()
        .unwrap();

    assert!(!output.status.success(), "foreign listener must refuse");
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("could not confirm it is its own proxy"),
        "{stderr}"
    );
    assert!(
        !args_file.exists(),
        "claude must never launch against an unverified listener"
    );
    // The credential-disclosure guarantee: the configured key was NEVER sent
    // to the unverified listener (only the random canary crosses the socket).
    let requests = request_log.lock().unwrap();
    assert!(
        requests.iter().all(|request| !request.contains("e2e-key")),
        "the configured proxy key leaked to an unverified listener: {requests:?}"
    );
    assert!(
        requests
            .iter()
            .any(|request| request.contains("Bearer canary-")),
        "the canary probe should still have been attempted"
    );
}

#[test]
fn stale_pidfile_for_a_dead_process_is_cleaned_without_signalling() {
    let temp = tempfile::tempdir().unwrap();
    let proxy_dir = temp.path().join("proxy");
    std::fs::create_dir_all(&proxy_dir).unwrap();
    std::fs::write(
        proxy_dir.join("cliproxyapi.pid"),
        r#"{"pid": 4000000, "binary": "/x/cliproxyapi"}"#,
    )
    .unwrap();

    let output = Command::cargo_bin("ovm-claudex")
        .unwrap()
        .arg("stop")
        .env("OVM_CLAUDEX_HOME", temp.path())
        .output()
        .unwrap();
    assert!(output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("stale pidfile"), "{stderr}");
    assert!(!proxy_dir.join("cliproxyapi.pid").exists());
}
