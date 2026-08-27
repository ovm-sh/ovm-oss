//! `ovm update` end to end: check, sweep, and the interactive chooser.
//!
//! The behaviours worth protecting are the ones a user or a script would
//! notice changing:
//!   - `--check` reports and installs nothing;
//!   - a non-terminal `ovm update` still sweeps, because scripts predate the
//!     prompt and must not start hanging on one;
//!   - the picker, under a real PTY, can decline everything and leave the
//!     machine exactly as it was.
//!
//! Codex is the fixture product: its source is a plain releases API, so two
//! versions can be served from one mockito server.

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, Server, ServerGuard};
use std::fs;
use std::io::Read;
use std::path::Path;
use std::time::{Duration, Instant};

const OLD: &str = "rust-v0.142.0";
const NEW: &str = "rust-v0.146.0";

fn make_tarball(entry_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = tar::Builder::new(encoder);
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder
            .append_data(&mut header, entry_name, contents)
            .expect("append");
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }
    buf
}

fn expected_codex_asset() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "codex-aarch64-apple-darwin.tar.gz"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "codex-x86_64-apple-darwin.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "codex-aarch64-unknown-linux-musl.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "codex-x86_64-unknown-linux-musl.tar.gz"
    }
}

fn expected_codex_entry() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "codex-aarch64-apple-darwin"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "codex-x86_64-apple-darwin"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "codex-aarch64-unknown-linux-musl"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "codex-x86_64-unknown-linux-musl"
    }
}

/// A releases API serving two Codex versions, so "latest" is genuinely ahead
/// of what the test installs first.
fn setup_two_version_mock() -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_codex_asset();
    let asset_entry = expected_codex_entry();

    let mut entries = Vec::new();
    for version in [NEW, OLD] {
        let body = make_tarball(asset_entry, b"#!/bin/sh\necho fake-codex\n");
        let size = body.len();
        server
            .mock("GET", format!("/download/{version}/{asset_name}").as_str())
            .with_status(200)
            .with_header("content-type", "application/octet-stream")
            .with_body(body)
            .expect_at_least(0)
            .create();

        let asset_url = format!("{}/assets/{version}/{asset_name}", server.url());
        let json = format!(
            r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}","size":{size}}}]}}"#
        );
        server
            .mock("GET", format!("/tags/{version}").as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(json.clone())
            .expect_at_least(0)
            .create();
        entries.push(json);
    }

    // `latest` and the paginated list both report NEW first.
    server
        .mock("GET", "/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(entries[0].clone())
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("per_page".into(), "100".into()),
            Matcher::UrlEncoded("page".into(), "1".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{}]", entries.join(",")))
        .expect_at_least(0)
        .create();
    server
        .mock("GET", "/")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("per_page".into(), "100".into()),
            Matcher::UrlEncoded("page".into(), "2".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .expect_at_least(0)
        .create();

    let base = server.url();
    (server, base)
}

/// A port that refuses instantly, so the npm and OVM-registry fast paths
/// fail fast and "latest" is decided by the mocked releases API alone. If a
/// fast path could reach the real network, the expected NEW version would go
/// stale the day upstream ships a newer release (it did: rust-v0.146.1).
const DEAD_URL: &str = "http://127.0.0.1:9";

fn ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_CODEX_RELEASES_URL", releases_url)
        .env("OVM_CODEX_NPM_REGISTRY_URL", DEAD_URL)
        .env("OVM_REGISTRY_BASE_URL", DEAD_URL)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

/// Install OLD and make it active — the starting point for every case below.
fn install_old(home: &Path, releases_url: &str) {
    ovm(home, releases_url)
        .args(["install", "codex", OLD])
        .assert()
        .success();
    ovm(home, releases_url)
        .args(["use", "codex", OLD])
        .assert()
        .success();
}

fn active_version(home: &Path) -> String {
    let link = home.join(".ovm/products/codex/current");
    fs::read_link(&link)
        .ok()
        .and_then(|target| {
            target
                .file_name()
                .map(|name| name.to_string_lossy().to_string())
        })
        .unwrap_or_default()
}

#[test]
fn check_reports_the_update_and_installs_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    let output = ovm(home.path(), &releases_url)
        .args(["update", "codex", "--check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(
        text.contains(OLD) && text.contains(NEW),
        "reports the move: {text}"
    );
    assert!(text.contains("available"), "labels it available: {text}");
    assert_eq!(
        active_version(home.path()),
        OLD,
        "--check must not change the active version"
    );
    assert!(
        !home
            .path()
            .join(format!(".ovm/products/codex/versions/{NEW}"))
            .exists(),
        "--check must not download anything"
    );
}

#[test]
fn a_piped_update_still_sweeps_so_scripts_keep_working() {
    // assert_cmd gives the child pipes, not a terminal — the same shape a CI
    // job or `ovm update | tee log` has. It must not wait for a keypress.
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    ovm(home.path(), &releases_url)
        .args(["update", "codex"])
        .timeout(Duration::from_secs(120))
        .assert()
        .success();

    assert_eq!(active_version(home.path()), NEW, "the sweep applied it");
}

#[test]
fn yes_applies_the_named_product_without_asking() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    ovm(home.path(), &releases_url)
        .args(["update", "codex", "--yes"])
        .timeout(Duration::from_secs(180))
        .assert()
        .success();

    assert_eq!(active_version(home.path()), NEW);
}

/// `--yes` answers the picker; it does not overrule a pin. `ovm use` pins, and
/// a sweep that quietly undid that would make the pin meaningless — naming the
/// product is still the way to say "move it anyway".
#[test]
fn yes_does_not_turn_a_sweep_into_a_pin_override() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url); // `ovm use` pins OLD

    let output = ovm(home.path(), &releases_url)
        .args(["update", "--yes"])
        .timeout(Duration::from_secs(180))
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(text.contains("pinned to"), "reports the pin: {text}");
    assert_eq!(
        active_version(home.path()),
        OLD,
        "a sweep must leave a pinned product where it is"
    );
}

#[test]
fn an_up_to_date_product_is_reported_not_reinstalled() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    ovm(home.path(), &releases_url)
        .args(["install", "codex", NEW])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["use", "codex", NEW])
        .assert()
        .success();

    let output = ovm(home.path(), &releases_url)
        .args(["update", "codex"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(text.contains("already latest"), "{text}");
    assert!(
        !text.contains("update(s) available"),
        "nothing to choose from: {text}"
    );
}

/// Drive the picker under a real PTY: decline everything, change nothing.
#[test]
fn the_picker_can_decline_and_leave_the_machine_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    let binary = assert_cmd::cargo::cargo_bin("ovm");
    let mut session = rexpect::spawn(
        &format!(
            "env HOME={home} OVM_DISABLE_BACKGROUND_REFRESH=1 OVM_CODEX_RELEASES_URL={url} \
             OVM_CODEX_NPM_REGISTRY_URL={DEAD_URL} OVM_REGISTRY_BASE_URL={DEAD_URL} \
             OVM_SKIP_SIGNATURE_VERIFY=1 {binary} update codex",
            home = home.path().display(),
            url = releases_url,
            binary = binary.display(),
        ),
        Some(120_000),
    )
    .expect("spawn under a pty");

    // The picker offers the update rather than installing it.
    session
        .exp_string("update(s) available")
        .expect("picker frame");
    // `n` clears every tick; Enter then confirms an empty selection.
    session.send("n").expect("send n");
    session.send_line("").expect("confirm");
    session
        .exp_string("Nothing selected")
        .expect("says it changed nothing");

    assert_eq!(
        active_version(home.path()),
        OLD,
        "declining must leave the active version alone"
    );
    assert!(
        !home
            .path()
            .join(format!(".ovm/products/codex/versions/{NEW}"))
            .exists(),
        "declining must not download anything"
    );
}

/// The same PTY, accepting: the default selection installs the update.
#[test]
fn the_picker_accepts_the_default_selection_with_one_keypress() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    let binary = assert_cmd::cargo::cargo_bin("ovm");
    let mut session = rexpect::spawn(
        &format!(
            "env HOME={home} OVM_DISABLE_BACKGROUND_REFRESH=1 OVM_CODEX_RELEASES_URL={url} \
             OVM_CODEX_NPM_REGISTRY_URL={DEAD_URL} OVM_REGISTRY_BASE_URL={DEAD_URL} \
             OVM_SKIP_SIGNATURE_VERIFY=1 {binary} update codex",
            home = home.path().display(),
            url = releases_url,
            binary = binary.display(),
        ),
        Some(180_000),
    )
    .expect("spawn under a pty");

    session
        .exp_string("update(s) available")
        .expect("picker frame");
    // Everything starts ticked, so Enter alone is the whole interaction.
    session.send_line("").expect("confirm");

    // Wait for the install to land rather than racing it.
    let deadline = Instant::now() + Duration::from_secs(150);
    while Instant::now() < deadline {
        if active_version(home.path()) == NEW {
            break;
        }
        std::thread::sleep(Duration::from_millis(250));
    }

    let mut rest = String::new();
    let _ = session.exp_eof().map(|text| rest = text);
    assert_eq!(
        active_version(home.path()),
        NEW,
        "accepting installs the update (tail: {rest})"
    );
}

/// A pin hides nothing from `--check`: the update is reported, labelled as
/// pinned, and still not installed.
#[test]
fn a_bare_check_shows_the_pinned_update_instead_of_hiding_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url); // `ovm use` pins OLD

    let output = ovm(home.path(), &releases_url)
        .args(["update", "--check"])
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(
        text.contains(NEW) && text.contains("(pinned)"),
        "the pinned update is visible and labelled: {text}"
    );
    assert_eq!(active_version(home.path()), OLD);
}

/// The bare sweep's picker offers a pinned update unticked: plain Enter must
/// leave the pin alone, so moving it always takes a deliberate keypress.
#[test]
fn the_sweep_picker_offers_a_pinned_update_unticked() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url); // `ovm use` pins OLD

    let binary = assert_cmd::cargo::cargo_bin("ovm");
    let mut session = rexpect::spawn(
        &format!(
            "env HOME={home} OVM_DISABLE_BACKGROUND_REFRESH=1 OVM_CODEX_RELEASES_URL={url} \
             OVM_CODEX_NPM_REGISTRY_URL={DEAD_URL} OVM_REGISTRY_BASE_URL={DEAD_URL} \
             OVM_SKIP_SIGNATURE_VERIFY=1 {binary} update",
            home = home.path().display(),
            url = releases_url,
            binary = binary.display(),
        ),
        Some(120_000),
    )
    .expect("spawn under a pty");

    session
        .exp_string("update(s) available")
        .expect("picker frame");
    session.exp_string("(pinned)").expect("labels the pin");
    session
        .exp_string("auto-update on launch")
        .expect("the settings section rides along");
    // The pinned row starts unticked and no setting moved, so Enter is a
    // no-op confirm.
    session.send_line("").expect("confirm");
    session
        .exp_string("Nothing selected")
        .expect("says it changed nothing");

    assert_eq!(
        active_version(home.path()),
        OLD,
        "confirming the defaults must not move a pin"
    );
}

/// Moving a policy row and confirming persists the change — the sweep screen
/// is also the settings screen.
#[test]
fn the_sweep_picker_saves_a_policy_change() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);

    let binary = assert_cmd::cargo::cargo_bin("ovm");
    let mut session = rexpect::spawn(
        &format!(
            "env HOME={home} OVM_DISABLE_BACKGROUND_REFRESH=1 OVM_CODEX_RELEASES_URL={url} \
             OVM_CODEX_NPM_REGISTRY_URL={DEAD_URL} OVM_REGISTRY_BASE_URL={DEAD_URL} \
             OVM_SKIP_SIGNATURE_VERIFY=1 {binary} update",
            home = home.path().display(),
            url = releases_url,
            binary = binary.display(),
        ),
        Some(120_000),
    )
    .expect("spawn under a pty");

    session
        .exp_string("auto-update on launch")
        .expect("settings section");
    // One update row (pinned Codex), then the policy rows: down once lands on
    // the first product's policy; space advances it `on → notify`.
    session.send("\x1b[B").expect("arrow down");
    session.send(" ").expect("cycle policy");
    session.send_line("").expect("confirm");
    // Wait for the process to finish so the config write has landed.
    let tail = session.exp_eof().expect("run finishes");
    assert!(tail.contains("notify"), "reports the policy move: {tail}");

    let config = fs::read_to_string(home.path().join(".ovm/config.json")).expect("config saved");
    assert!(
        config.contains("notify"),
        "the policy change was persisted: {config}"
    );
    assert_eq!(
        active_version(home.path()),
        OLD,
        "a settings-only confirm installs nothing"
    );
}

/// A corrupt config fails every product check (each one loads it), and the
/// settings section must degrade to a warning rather than abort before the
/// summary — the run still ends as a reported failure, never as success or
/// a picker waiting on keys it can do nothing with.
#[test]
fn a_corrupt_config_reports_failure_instead_of_aborting_or_succeeding() {
    let home = tempfile::tempdir().expect("tempdir");
    let (_server, releases_url) = setup_two_version_mock();
    install_old(home.path(), &releases_url);
    fs::write(home.path().join(".ovm/config.json"), "{not json").expect("corrupt the config");

    let binary = assert_cmd::cargo::cargo_bin("ovm");
    let mut session = rexpect::spawn(
        &format!(
            "env HOME={home} OVM_DISABLE_BACKGROUND_REFRESH=1 OVM_CODEX_RELEASES_URL={url} \
             OVM_CODEX_NPM_REGISTRY_URL={DEAD_URL} OVM_REGISTRY_BASE_URL={DEAD_URL} \
             OVM_SKIP_SIGNATURE_VERIFY=1 {binary} update",
            home = home.path().display(),
            url = releases_url,
            binary = binary.display(),
        ),
        Some(120_000),
    )
    .expect("spawn under a pty");

    session
        .exp_string("auto-update settings unavailable")
        .expect("the settings section explains itself instead of aborting");
    session
        .exp_string("failed")
        .expect("the summary still counts the failures");
    // The closing error only prints on the Err path, which is what makes the
    // process exit non-zero — an all-failed sweep must never read as success.
    session
        .exp_string("Could not update")
        .expect("the run ends as a failure");
    assert_eq!(active_version(home.path()), OLD);
}

/// A guard for the flag itself: `--check` and `--yes` contradict each other,
/// and clap must say so rather than silently preferring one.
#[test]
fn check_and_yes_cannot_be_combined() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home.path())
        .args(["update", "--check", "--yes"])
        .assert()
        .failure();
}

/// Reading the whole file keeps clippy quiet about the unused import in some
/// cfg combinations while documenting what `Read` is here for.
#[allow(dead_code)]
fn read_all(mut source: impl Read) -> String {
    let mut buffer = String::new();
    let _ = source.read_to_string(&mut buffer);
    buffer
}
