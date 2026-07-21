//! Launch hot-path performance & hang guards.
//!
//! When launch auto-update is off, `ovm <product>` must not block the foreground
//! on the update service: the update banner reads from the local cache and the
//! registry refresh is spawned *detached* (`refresh_cache.rs` → `Stdio::null` +
//! `.spawn()`). These tests seed a fake active install for each product and
//! assert the pinned launch path returns promptly under both network-failure modes:
//!   - *bad internet*: a black-hole ("tarpit") socket that accepts connections
//!     but never responds, so a naive client stalls until its read timeout.
//!   - *no internet*: a closed port that refuses connections immediately
//!     (ECONNREFUSED), the fast-fail you get with the network down.
//!
//! This is the regression guard for the "launch hangs when the network is bad"
//! failures: if anyone reintroduces a synchronous network fetch on the launch
//! pinned foreground path (or makes the background refresh blocking), the tarpit
//! makes that fetch stall on the registry's timeout and these budgets fail.

use assert_cmd::Command;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::thread;
use std::time::{Duration, Instant};

/// (canonical name, a representative installed version) for each product.
const PRODUCTS: [(&str, &str); 3] = [
    ("claude", "2.1.112"),
    ("codex", "rust-v0.130.0"),
    ("pi", "0.67.6"),
];

/// The launch foreground only reads the local cache; a correct launch finishes
/// well under a second. Kept below the registry client's 5s timeout so a
/// regression that synchronously fetches against the tarpit blows the budget.
const LAUNCH_BUDGET: Duration = Duration::from_secs(3);
static PERF_LOCK: Mutex<()> = Mutex::new(());

/// Where each product's active binary lives on disk, mirroring
/// `ProductDirs::resolved_binary`.
fn active_binary_path(home: &Path, product: &str, version: &str) -> PathBuf {
    let version_dir = home
        .join(".ovm/products")
        .join(product)
        .join("versions")
        .join(version);
    match product {
        "claude" => version_dir.join("native/claude"),
        "codex" => version_dir.join("release/bin/codex"),
        "pi" => version_dir.join("release/bundle/pi/pi"),
        other => panic!("unknown product {other}"),
    }
}

/// Seed a fake active install: a shell-script binary at the resolved path plus
/// the `current` symlink so `ovm <product>` resolves and execs it. The script
/// echoes its args so the test can confirm launch reached exec.
fn seed_active(home: &Path, product: &str, version: &str) {
    let binary = active_binary_path(home, product, version);
    fs::create_dir_all(binary.parent().expect("binary parent")).expect("mkdir version dir");
    fs::write(
        &binary,
        format!("#!/bin/sh\necho \"{product} {version} args=$*\"\n"),
    )
    .expect("write fake binary");
    let mut perms = fs::metadata(&binary).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&binary, perms).expect("chmod");

    let version_dir = home
        .join(".ovm/products")
        .join(product)
        .join("versions")
        .join(version);
    let source_root = match product {
        "claude" => version_dir.join("native"),
        "codex" | "pi" => version_dir.join("release"),
        other => panic!("unknown product {other}"),
    };
    fs::write(source_root.join(".complete"), "").expect("write completion marker");

    let product_dir = home.join(".ovm/products").join(product);
    let current = product_dir.join("current");
    let target = product_dir.join("versions").join(version);
    let _ = fs::remove_file(&current);
    std::os::unix::fs::symlink(&target, &current).expect("current symlink");
}

/// A socket that accepts connections and holds them open forever without
/// responding — simulates a wedged/black-holed update service (i.e. *bad*
/// internet: the TCP handshake succeeds but no bytes ever come back, so the
/// client stalls until its read timeout). Returns its `http://127.0.0.1:<port>`
/// base URL. The listener thread runs for the process lifetime; held streams are
/// parked so the peer sees a hang, not a connection reset.
fn spawn_tarpit() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tarpit");
    let addr = listener.local_addr().expect("tarpit addr");
    thread::spawn(move || {
        let mut parked = Vec::new();
        for stream in listener.incoming() {
            match stream {
                Ok(stream) => parked.push(stream),
                Err(_) => break,
            }
        }
    });
    format!("http://{addr}")
}

/// A URL pointing at a port nobody is listening on — simulates *no* internet:
/// every connection attempt is refused immediately (ECONNREFUSED), the same
/// fast-fail you get when a host is down or the network is unreachable. We bind
/// to grab a free port, capture its address, then drop the listener so the port
/// is closed. (Connection-refused is instant, so this is the easy case — but it
/// must still never bubble a hard error onto the launch foreground.)
fn dead_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind for free port");
    let addr = listener.local_addr().expect("dead-port addr");
    drop(listener);
    format!("http://{addr}")
}

/// Run `ovm <product> --version` against `update_service`, assert it reached
/// exec under budget, and return how long it took. Prints the per-product load
/// time so `cargo test -- --nocapture` shows exactly where the time goes.
fn time_launch(scenario: &str, product: &str, version: &str, update_service: &str) -> Duration {
    let home = tempfile::tempdir().expect("tempdir");
    seed_active(home.path(), product, version);

    let start = Instant::now();
    let assert = ovm(home.path(), update_service)
        .args([product, "--version"])
        .assert()
        .success();
    let elapsed = start.elapsed();

    eprintln!("[launch_perf] {scenario:<22} {product:<6} {elapsed:>8.2?}");

    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
    assert!(
        stdout.contains("args=--version"),
        "{product} launch did not reach exec (stdout: {stdout:?})"
    );
    assert!(
        elapsed <= LAUNCH_BUDGET,
        "{product} launch took {elapsed:?} against the {scenario} service, \
         expected <= {LAUNCH_BUDGET:?} — the foreground is blocking on the network"
    );
    elapsed
}

/// `ovm` invocation with the home isolated and every upstream pointed at
/// `update_service` so no test can touch the real internet.
fn ovm(home: &Path, update_service: &str) -> Command {
    ensure_test_config(home);
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("NO_COLOR", "1")
        .env("OVM_REGISTRY_BASE_URL", update_service)
        .env("OVM_CODEX_RELEASES_URL", update_service)
        .env("OVM_PI_RELEASES_URL", update_service)
        .env("OVM_NPM_PACKAGE_URL", update_service)
        .env("OVM_GITHUB_API_URL", update_service)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT")
        // Hard backstop: a truly unbounded hang fails the test instead of
        // wedging the whole suite.
        .timeout(Duration::from_secs(20));
    cmd
}

fn ensure_test_config(home: &Path) {
    let config = home.join(".ovm/config.json");
    if config.exists() {
        return;
    }
    fs::create_dir_all(config.parent().expect("config parent")).expect("mkdir config parent");
    fs::write(
        config,
        r#"{
            "checkForUpdates": false,
            "autoUpdate": { "default": "off" },
            "cleanup": { "retention": "never" }
        }"#,
    )
    .expect("write test config");
}

/// Cold cache + wedged update service: launch must not block on it. Exercises
/// the shared foreground path (update banner, schema guard) plus the detached
/// background-refresh spawn, for every product.
#[test]
fn launch_does_not_block_when_update_service_is_unreachable() {
    let _guard = PERF_LOCK.lock().expect("perf lock");
    let tarpit = spawn_tarpit();
    for (product, version) in PRODUCTS {
        time_launch("bad-internet (hang)", product, version, &tarpit);
    }
}

/// No internet: every upstream is pointed at a closed port so connections are
/// refused outright (ECONNREFUSED). The launch must shrug it off and reach exec
/// just as fast — a refused connection must never surface as a hard launch
/// error. Complements the tarpit test: that covers a service that hangs, this
/// covers one that isn't there at all.
#[test]
fn launch_does_not_fail_when_offline() {
    let _guard = PERF_LOCK.lock().expect("perf lock");
    let offline = dead_port_url();
    for (product, version) in PRODUCTS {
        time_launch("no-internet (refused)", product, version, &offline);
    }
}

/// Warm cache: with a fresh version index already on disk, launch reads it
/// locally and never contacts the update service — fast and silent even when
/// the service is wedged.
#[test]
fn cached_launch_reads_local_index_without_network() {
    let _guard = PERF_LOCK.lock().expect("perf lock");
    let tarpit = spawn_tarpit();

    for (product, version) in PRODUCTS {
        let home = tempfile::tempdir().expect("tempdir");
        seed_active(home.path(), product, version);

        // Warm the version-index cache from a fast mock registry so the launch
        // below has a fresh index and skips the background refresh entirely.
        let mut registry = mockito::Server::new();
        registry
            .mock("GET", format!("/{product}.json").as_str())
            .with_status(200)
            .with_body(format!(
                r#"{{"versions":[{{"version":"{version}","date":"2026-05-13"}}]}}"#
            ))
            .create();
        ovm(home.path(), &registry.url())
            .args(["ls", product])
            .assert()
            .success();

        // Now launch against the wedged service: the warm cache means zero
        // network, so it stays fast and emits no "unreachable" fallback.
        let start = Instant::now();
        let assert = ovm(home.path(), &tarpit)
            .args([product, "--version"])
            .assert()
            .success();
        let elapsed = start.elapsed();

        eprintln!(
            "[launch_perf] {:<22} {product:<6} {elapsed:>8.2?}",
            "warm-cache (tarpit)"
        );

        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(
            elapsed <= LAUNCH_BUDGET,
            "{product} cached launch took {elapsed:?}, expected <= {LAUNCH_BUDGET:?}"
        );
        assert!(
            !stderr.contains("unreachable") && !stderr.contains("Could not reach"),
            "{product} cached launch hit the network (stderr: {stderr:?})"
        );
    }
}
