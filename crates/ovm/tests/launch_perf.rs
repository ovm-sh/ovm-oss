//! Launch hot-path performance & hang guards.
//!
//! Under the DEFAULT config — auto-update `on`, update checks enabled —
//! `ovm <product>` must not block the foreground on the update service: both the
//! update banner and the auto-update decision read the local cache, and the
//! registry refresh is spawned *detached* (`refresh_cache.rs` → `Stdio::null` +
//! `.spawn()`). These tests seed a fake active install for each product and
//! assert the launch path returns promptly under both network-failure modes:
//!   - *bad internet*: a black-hole ("tarpit") socket that accepts connections
//!     but never responds, so a naive client stalls until its read timeout.
//!   - *no internet*: a closed port that refuses connections immediately
//!     (ECONNREFUSED), the fast-fail you get with the network down.
//!
//! This is the regression guard for the "launch hangs when the network is bad"
//! failures: if anyone reintroduces a synchronous network fetch on the launch
//! foreground path (or makes the background refresh blocking), the tarpit makes
//! that fetch stall on the registry's timeout and these budgets fail — and the
//! stderr check in `time_launch` catches the variant that fails fast instead.

use assert_cmd::Command;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{mpsc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

/// (canonical name, a representative installed version) for each product.
const PRODUCTS: [(&str, &str); 4] = [
    ("claude", "2.1.112"),
    ("codex", "rust-v0.130.0"),
    ("pi", "0.67.6"),
    ("qm", "0.1.4"),
];

/// The launch foreground only reads the local cache; a correct launch finishes
/// well under a second. Kept below the registry client's 5s timeout so a
/// regression that synchronously fetches against the tarpit blows the budget.
const LAUNCH_BUDGET: Duration = Duration::from_secs(3);
/// Samples taken per product, of which the median must come in under budget.
///
/// A real regression blows the budget on *every* attempt — a synchronous fetch
/// is unconditional, and each attempt starts from an identical cold tempdir — so
/// widening this costs no sensitivity. Load noise, by contrast, is transient:
/// with the OS first-exec toll paid off the clock (see [`warm_first_exec`]) a
/// launch measures ~7ms, but under a saturated machine a whole process spawn can
/// still occasionally starve for seconds. Five samples let the median absorb two
/// such outliers instead of one; measured over 54 samples under a full
/// `cargo test --workspace` plus 12 processes churning fresh executables, 52
/// landed in 5.6-13.2ms and 2 starved — a rate at which median-of-3 is not quite
/// enough and median-of-5 has ample room.
const LAUNCH_ATTEMPTS: u32 = 5;
static PERF_LOCK: Mutex<()> = Mutex::new(());

/// Serialize the timing tests, tolerating a poisoned lock.
///
/// These tests panic on a blown budget, and a panic while holding the lock
/// poisons it. `.expect()` then fails every remaining test with `PoisonError`
/// instead of its own verdict — so one timing failure under load reported as
/// three, and the one real reason was nowhere in the output. The mutex guards
/// nothing but exclusivity: there is no shared state a panic could corrupt, so
/// recovering the guard is safe and each test gets to report what it actually
/// measured.
fn perf_guard() -> std::sync::MutexGuard<'static, ()> {
    PERF_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
        "qm" => version_dir.join("release/bundle/package/dist/bin/qm.js"),
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
    warm_first_exec(&binary);

    let version_dir = home
        .join(".ovm/products")
        .join(product)
        .join("versions")
        .join(version);
    let source_root = match product {
        "claude" => version_dir.join("native"),
        "codex" | "pi" | "qm" => version_dir.join("release"),
        other => panic!("unknown product {other}"),
    };
    match product {
        "pi" => fs::write(source_root.join("bundle/pi/package.json"), "{}").expect("pi package"),
        "qm" => {
            fs::write(source_root.join("bundle/package/package.json"), "{}").expect("qm package")
        }
        "claude" | "codex" => {}
        other => panic!("unknown product {other}"),
    }
    fs::write(source_root.join(".complete"), "").expect("write completion marker");

    let product_dir = home.join(".ovm/products").join(product);
    let current = product_dir.join("current");
    let target = product_dir.join("versions").join(version);
    let _ = fs::remove_file(&current);
    std::os::unix::fs::symlink(&target, &current).expect("current symlink");
}

/// Run the freshly written fake binary once, *before* any clock starts, so the
/// timed launch execs a file the OS has already assessed.
///
/// This is not a warm-up for OVM — OVM is not involved. macOS assesses every
/// executable the **first** time that particular file is exec'd (Gatekeeper /
/// XProtect, via `syspolicyd`), and the seeded binary is a brand-new file in a
/// brand-new tempdir on every attempt, so every attempt used to pay that toll
/// inside the measurement. Measured on an idle Apple Silicon machine, the toll
/// is ~370ms; with several processes exec'ing fresh files at once — exactly what
/// a full `cargo test --workspace` does, since many integration tests seed fake
/// binaries — the daemon serializes and the same exec takes **1.7-4.5s**. A
/// shell script exec'd with no OVM binary anywhere in the picture reproduces it
/// identically, which is how we know it is not ours.
///
/// That was the whole flake: `launch_does_not_fail_when_offline` failed a full
/// workspace run at a 6.08s median against a *connection-refused* port, where
/// a real network round trip costs microseconds. With the file pre-assessed the
/// same launch measures a flat 28-67ms under every load we could produce —
/// roughly 75x of headroom under [`LAUNCH_BUDGET`] instead of 8x.
///
/// Nothing about the guard's sensitivity changes: this removes a constant that
/// belongs to the OS, not a network cost. A foreground fetch against the tarpit
/// still stalls on the registry client's 5s timeout (up to ~50s for Codex's
/// npm → registry → GitHub chain) and still blows the budget on every attempt.
///
/// Bounded and non-fatal. This exec runs *outside* the 20s backstop that
/// [`ovm`] puts on the measured launch, so an `output()` here — which waits
/// forever — would let a wedged `syspolicyd` hang the whole suite with no
/// verdict at all, the worst possible failure mode for a hang guard. The
/// deadline is deliberately loose (this warms, it does not measure; the worst
/// honest assessment observed was 4.5s), and expiry is a shrug rather than a
/// panic: an unwarmed file only risks re-introducing the OS toll *inside* the
/// measurement, and the budget assertion is exactly what says so.
///
/// The whole spawn-and-supervise runs on a helper thread, and the caller waits
/// on a channel rather than joining it. This is not concurrency for speed — it
/// is what makes the deadline actually cover the exec. `spawn()` is the call the
/// OS assessment blocks *inside*: `syspolicyd` does its work before the child
/// exists, so a deadline started after `spawn()` returns cannot bound the part
/// that hangs. Only a thread boundary can, because the waiting thread is not the
/// one in the syscall.
///
/// What is bounded and what is not, precisely: the *test thread* is bounded, at
/// `WARMUP_DEADLINE` + `WARMUP_JOIN_MARGIN`, unconditionally. The helper
/// thread is not — a thread wedged inside `spawn()` cannot be cancelled, so on
/// timeout we abandon it rather than join it. That leak is acceptable here and
/// only here: it costs one thread stack in a test process that is about to exit
/// anyway, and the alternative is a suite that hangs forever with no verdict.
/// If the child ever does materialize, the same abandoned thread is still
/// supervising it and still kills it on its own deadline — the leak defers the
/// cleanup, it does not skip it.
fn warm_first_exec(binary: &Path) {
    const WARMUP_DEADLINE: Duration = Duration::from_secs(60);
    const WARMUP_POLL: Duration = Duration::from_millis(10);
    /// Slack on top of the helper's own deadline, so a helper that is merely
    /// finishing its last poll reports in normally instead of being abandoned
    /// on a photo finish.
    const WARMUP_JOIN_MARGIN: Duration = Duration::from_secs(5);

    let binary = binary.to_path_buf();
    let (done_tx, done_rx) = mpsc::channel();

    thread::spawn(move || {
        // stdio to null: the fake binary echoes its args, and this run is not
        // one the test inspects.
        let spawned = std::process::Command::new(&binary)
            .arg("--warmup")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        // The `Child` never leaves this thread: whoever spawned it is whoever
        // kills it, so the kill path survives the caller walking away.
        let Ok(mut child) = spawned else {
            let _ = done_tx.send(());
            return;
        };

        let start = Instant::now();
        loop {
            match child.try_wait() {
                // Exited (however it exited) or unwaitable — either way done.
                Ok(Some(_)) | Err(_) => break,
                Ok(None) => {}
            }
            if start.elapsed() >= WARMUP_DEADLINE {
                let _ = child.kill();
                let _ = child.wait();
                break;
            }
            thread::sleep(WARMUP_POLL);
        }
        // Best-effort: if the caller already timed out, the receiver is gone and
        // this send fails, which is exactly the abandoned-helper path.
        let _ = done_tx.send(());
    });

    // Both outcomes are a shrug. `Ok`/`Disconnected` mean the helper is done (or
    // panicked, which also warms nothing and also must not fail the test);
    // `Timeout` means it is stuck and we proceed unwarmed. Either way the budget
    // assertion is what speaks if the OS toll lands inside a measurement.
    let _ = done_rx.recv_timeout(WARMUP_DEADLINE + WARMUP_JOIN_MARGIN);
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
    // Median-of-N, not best-of-N. A single sample captures scheduling noise
    // (cold paging, a parallel test hogging cores) — but taking the *fastest*
    // attempt would pass a launch that blocks intermittently, e.g. a race where
    // two of three runs stall on the tarpit. Requiring the median under budget
    // tolerates one slow outlier while still failing when blocking is the
    // common case.
    //
    // The median is insurance, not the load-bearing part: what actually made
    // this the suite's only flaky test was the OS first-exec assessment of the
    // freshly seeded binary, which `seed_active` now pays off the clock — see
    // `warm_first_exec`. Note the samples printed on failure are sorted, so a
    // failure message never shows which attempt was which.
    let mut samples = Vec::with_capacity(LAUNCH_ATTEMPTS as usize);
    for attempt in 1..=LAUNCH_ATTEMPTS {
        let home = tempfile::tempdir().expect("tempdir");
        seed_active(home.path(), product, version);

        let start = Instant::now();
        let assert = ovm(home.path(), update_service)
            .args([product, "--version"])
            .assert()
            .success();
        let elapsed = start.elapsed();

        eprintln!("[launch_perf] {scenario:<22} {product:<6} attempt {attempt} {elapsed:>8.2?}");

        let stdout = String::from_utf8_lossy(&assert.get_output().stdout).into_owned();
        assert!(
            stdout.contains("args=--version"),
            "{product} launch did not reach exec (stdout: {stdout:?})"
        );
        // Timing alone cannot catch a regression against a *refused* connection:
        // ECONNREFUSED comes back instantly, so a synchronous fetch would fail
        // fast and stay under budget while still being a network round trip on
        // every launch. Its failure message is the tell.
        let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
        assert!(
            !stderr.contains("Could not check for")
                && !stderr.contains("Could not reach update service")
                && !stderr.contains("Upstream unreachable"),
            "{product} launch consulted the {scenario} update service on the foreground \
             (stderr: {stderr:?})"
        );
        samples.push(elapsed);
    }

    samples.sort_unstable();
    let median = samples[samples.len() / 2];
    assert!(
        median <= LAUNCH_BUDGET,
        "{product} launch median was {median:?} over {LAUNCH_ATTEMPTS} attempts against the \
         {scenario} service (samples: {samples:?}), expected <= {LAUNCH_BUDGET:?} — the \
         foreground is blocking on the network"
    );
    median
}

/// `ovm` invocation with the home isolated and every upstream pointed at
/// `update_service` so no test can touch the real internet.
fn ovm(home: &Path, update_service: &str) -> Command {
    ensure_test_config(home);
    let runtime_bin = ensure_test_node(home);
    let mut path = vec![runtime_bin];
    path.extend(std::env::split_paths(
        &std::env::var_os("PATH").unwrap_or_default(),
    ));
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("PATH", std::env::join_paths(path).expect("test PATH"))
        .env("NO_COLOR", "1")
        .env("OVM_REGISTRY_BASE_URL", update_service)
        .env("OVM_CODEX_RELEASES_URL", update_service)
        .env("OVM_CODEX_NPM_REGISTRY_URL", update_service)
        .env("OVM_PI_RELEASES_URL", update_service)
        .env("OVM_PI_NPM_REGISTRY_URL", update_service)
        .env("OVM_NPM_PACKAGE_URL", update_service)
        .env("OVM_QM_NPM_PACKAGE_URL", update_service)
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

fn ensure_test_node(home: &Path) -> PathBuf {
    let bin = home.join(".test-bin");
    let node = bin.join("node");
    if node.exists() {
        return bin;
    }
    fs::create_dir_all(&bin).expect("test runtime bin");
    fs::write(&node, "#!/bin/sh\nprintf 'v24.0.0\\n'\n").expect("fake node");
    let mut permissions = fs::metadata(&node).expect("node metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&node, permissions).expect("node permissions");
    bin
}

/// The config under test is deliberately the DEFAULT one: `autoUpdate` stays at
/// its default `on` for existing products and QM keeps its risk-based `notify`
/// default; `checkForUpdates` stays `true`. These guards therefore measure what
/// every new user actually runs.
///
/// This file used to pin `autoUpdate: off`, which quietly excused the only
/// policy that fetched: under `on` an unpinned launch called
/// `latest_available_version()` straight out to the network — no cache, no TTL —
/// so a wedged update service stalled every `claude` for 15s and every `codex`
/// for up to ~50s. The tarpit could not see it because the tests never enabled
/// the policy that did it. Only `cleanup.retention` is overridden, to keep
/// launch-time pruning out of the measurement.
fn ensure_test_config(home: &Path) {
    let config = home.join(".ovm/config.json");
    if config.exists() {
        return;
    }
    fs::create_dir_all(config.parent().expect("config parent")).expect("mkdir config parent");
    fs::write(config, r#"{ "cleanup": { "retention": "never" } }"#).expect("write test config");
}

/// Cold cache + wedged update service: launch must not block on it. Exercises
/// the shared foreground path (update banner, schema guard) plus the detached
/// background-refresh spawn, for every product.
#[test]
fn launch_does_not_block_when_update_service_is_unreachable() {
    let _guard = perf_guard();
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
    let _guard = perf_guard();
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
    let _guard = perf_guard();
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
        // Sampled and medianed like the cold-cache guards above — a single
        // measurement against a 3s budget is one starved process spawn away
        // from a false failure, and the warm home is reusable, so the extra
        // samples cost only the launches themselves.
        let mut samples = Vec::with_capacity(LAUNCH_ATTEMPTS as usize);
        for _ in 0..LAUNCH_ATTEMPTS {
            let start = Instant::now();
            let assert = ovm(home.path(), &tarpit)
                .args([product, "--version"])
                .assert()
                .success();
            samples.push(start.elapsed());

            // Checked on every sample, not just the last: this is the assertion
            // that catches a fetch which fails fast instead of hanging, and one
            // launch out of five reaching the network is still a regression.
            let stderr = String::from_utf8_lossy(&assert.get_output().stderr).into_owned();
            assert!(
                !stderr.contains("unreachable") && !stderr.contains("Could not reach"),
                "{product} cached launch hit the network (stderr: {stderr:?})"
            );
        }
        // Held before sorting: unlike the cold-cache guards, every sample here
        // shares one home, so only the first launch is a launch on a home that
        // nothing has launched from yet. A regression that costs seconds *once*
        // per home — a lazy cache migration, a one-time index rewrite — is
        // invisible to the median (four fast samples outvote it) but is a real
        // per-user cost, paid on the launch a user actually notices. The
        // single-sample form this test used to have caught that for free;
        // asserting the first sample keeps it. It is stable now for the same
        // reason the median is: `warm_first_exec` pays the OS first-exec toll
        // off the clock, and that toll — not OVM — was the original flake.
        let first = samples[0];
        samples.sort_unstable();
        let elapsed = samples[samples.len() / 2];

        eprintln!(
            "[launch_perf] {:<22} {product:<6} first {first:>8.2?} median {elapsed:>8.2?}",
            "warm-cache (tarpit)"
        );

        assert!(
            first <= LAUNCH_BUDGET,
            "{product} first cached launch on a fresh home took {first:?} \
             (samples: {samples:?}), expected <= {LAUNCH_BUDGET:?} — something on the launch \
             path is doing once-per-home work in the foreground"
        );
        assert!(
            elapsed <= LAUNCH_BUDGET,
            "{product} cached launch median was {elapsed:?} over {LAUNCH_ATTEMPTS} attempts \
             (samples: {samples:?}), expected <= {LAUNCH_BUDGET:?}"
        );
    }
}
