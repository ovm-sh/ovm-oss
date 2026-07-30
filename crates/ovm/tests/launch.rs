//! Launch passthrough tests — verify `ovm <product>` execs the active binary
//! with arguments passed through, and that it exits with the binary's exit code.
//!
//! We install a fake `codex` binary (a shell script) through the mocked install
//! flow, activate it, then invoke `ovm codex` with various args.

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, Server, ServerGuard};
use std::fs;
use std::path::Path;
use tar::Builder;

fn make_tarball(entry_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = Builder::new(encoder);

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

fn ovm(home: &Path, releases_url: &str) -> Command {
    ensure_test_config(home);
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_CODEX_RELEASES_URL", releases_url)
        .env("OVM_REGISTRY_BASE_URL", releases_url)
        .env("OVM_CODEX_NPM_REGISTRY_URL", releases_url)
        // Test fixtures are unsigned fake binaries; skip codesign verification.
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1");
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

/// Install a fake codex binary (shell script) + activate it. Returns the server guard.
fn install_fake_codex(home: &Path, version: &str, script: &str) -> (ServerGuard, String) {
    let mut server = mockito::Server::new();
    let asset_name = expected_codex_asset();
    let entry = expected_codex_entry();
    let tarball = make_tarball(entry, script.as_bytes());

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_body(tarball)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/{version}").as_str())
        .with_status(200)
        .with_body(release_json.clone())
        .create();

    server
        .mock("GET", "/latest")
        .with_status(200)
        .with_body(release_json)
        .create();

    // Also mock the list endpoint so other commands don't fail
    server
        .mock("GET", "/")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body("[]")
        .create();

    let base = server.url();

    ovm(home, &base)
        .args(["install", "codex", version])
        .assert()
        .success();
    ovm(home, &base)
        .args(["use", "codex", version])
        .assert()
        .success();

    (server, base)
}

fn install_fake_claude(home: &Path, version: &str, script: &str) {
    let binary = home
        .join(".ovm/products/claude/versions")
        .join(version)
        .join("native/claude");
    fs::create_dir_all(binary.parent().expect("binary parent")).expect("create dirs");
    fs::write(&binary, script).expect("write fake claude");
    fs::write(
        binary.parent().expect("binary parent").join(".complete"),
        "",
    )
    .expect("write completion marker");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&binary).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary, perms).expect("chmod");
    }

    ovm(home, "http://127.0.0.1:9")
        .args(["use", "claude", version])
        .assert()
        .success();
}

fn write_fake_codex_binary(home: &Path, version: &str, script: &str) {
    let binary = home
        .join(".ovm/products/codex/versions")
        .join(version)
        .join("release/bin/codex");
    fs::create_dir_all(binary.parent().expect("binary parent")).expect("create dirs");
    fs::write(&binary, script).expect("write fake codex");
    fs::write(
        binary
            .parent()
            .expect("bin parent")
            .parent()
            .expect("release parent")
            .join(".complete"),
        "",
    )
    .expect("write completion marker");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(&binary).expect("metadata").permissions();
        perms.set_mode(0o755);
        fs::set_permissions(&binary, perms).expect("chmod");
    }
}

#[test]
fn launch_execs_active_binary_with_args_passed_through() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";

    // Fake "codex" is a shell script that echoes its args.
    let script = "#!/bin/sh\necho 'launched'\necho \"args=$*\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .args(["codex", "hello", "world"])
        .assert()
        .success()
        .stdout(predicates::str::contains("launched"))
        .stdout(predicates::str::contains("args=hello world"));
}

#[test]
fn launch_propagates_nonzero_exit_code() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";

    let script = "#!/bin/sh\nexit 42\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url).arg("codex").assert().code(42);
}

#[test]
fn launch_sets_ovm_env_vars() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";

    // The child prints OVM_* vars it sees
    let script = "#!/bin/sh\necho \"product=$OVM_PRODUCT\"\necho \"version=$OVM_VERSION\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .arg("codex")
        .assert()
        .success()
        .stdout(predicates::str::contains("product=codex"))
        .stdout(predicates::str::contains(format!("version={}", version)));
}

#[test]
fn launch_respects_ovm_version_override() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";

    let script = "#!/bin/sh\necho \"OVM_VERSION=$OVM_VERSION\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    // Explicit --ovm-version= override should pin to that version
    ovm(home.path(), &url)
        .args(["codex", "--ovm-version", version])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "OVM_VERSION={}",
            version
        )));
}

#[test]
fn launch_aliases_cc_cx_work() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";

    let script = "#!/bin/sh\necho active\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    // `cx` alias dispatches to Codex
    ovm(home.path(), &url)
        .arg("cx")
        .assert()
        .success()
        .stdout(predicates::str::contains("active"));
}

#[test]
fn codex_yolo_passes_current_dangerous_mode_flag() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";

    let script = "#!/bin/sh\necho \"args=$*\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .args(["codex", "--yolo", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "args=--dangerously-bypass-approvals-and-sandbox hello",
        ));
}

#[test]
fn cxy_alias_launches_codex_with_yolo_enabled() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";

    let script = "#!/bin/sh\necho \"args=$*\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .args(["cxy", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "args=--dangerously-bypass-approvals-and-sandbox hello",
        ));
}

#[test]
fn ccy_alias_launches_claude_with_yolo_enabled() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "2.1.91";

    let script = "#!/bin/sh\necho \"args=$*\"\n";
    install_fake_claude(home.path(), version, script);

    ovm(home.path(), "http://127.0.0.1:9")
        .args(["ccy", "hello"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "args=--dangerously-skip-permissions hello",
        ));
}

#[test]
fn inert_native_tree_does_not_emit_reclaim_warning_or_block_launch() {
    use predicates::prelude::PredicateBooleanExt;

    let home = tempfile::tempdir().expect("tempdir");
    let version = "2.1.91";
    install_fake_claude(home.path(), version, "#!/bin/sh\necho child-ran\n");
    fs::write(
        home.path().join(".claude.json"),
        r#"{"installMethod":"global","autoUpdates":false}"#,
    )
    .expect("write Claude config");
    let native_tree = home.path().join(".local/share/claude/versions");
    fs::create_dir_all(&native_tree).expect("native tree");
    fs::write(native_tree.join(version), "native debris").expect("native debris");

    ovm(home.path(), "http://127.0.0.1:9")
        .arg("ccy")
        .assert()
        .success()
        .stdout(predicates::str::contains("child-ran"))
        .stderr(predicates::str::contains("can reclaim version control").not());
}

#[test]
fn armed_native_method_warns_but_still_launches_child() {
    use predicates::prelude::PredicateBooleanExt;

    let home = tempfile::tempdir().expect("tempdir");
    let version = "2.1.91";
    install_fake_claude(home.path(), version, "#!/bin/sh\necho child-ran\n");
    fs::write(
        home.path().join(".claude.json"),
        r#"{"installMethod":"native"}"#,
    )
    .expect("write Claude config");

    ovm(home.path(), "http://127.0.0.1:9")
        .arg("ccy")
        .assert()
        .success()
        .stdout(predicates::str::contains("child-ran"))
        .stderr(
            predicates::str::contains("can reclaim version control")
                .and(predicates::str::contains("ovm doctor claude --fix")),
        );
}

/// Shared setup for the pin/auto-update pair below: two installed releases,
/// auto-update `on` for codex, and the older release explicitly activated.
fn setup_pinned_old_release(home: &Path, old_version: &str, latest_version: &str) {
    write_fake_codex_binary(
        home,
        old_version,
        "#!/bin/sh\necho \"version=$OVM_VERSION\"\n",
    );
    write_fake_codex_binary(
        home,
        latest_version,
        "#!/bin/sh\necho \"version=$OVM_VERSION\"\n",
    );

    fs::write(
        home.join(".ovm/config.json"),
        r#"{
            "checkForUpdates": false,
            "autoUpdate": {
                "default": "off",
                "codex": "on"
            }
        }"#,
    )
    .expect("write config");

    ovm(home, "http://127.0.0.1:9")
        .args(["use", "codex", old_version])
        .assert()
        .success()
        .stderr(predicates::str::contains("Pinned"));
}

#[test]
fn launch_respects_explicit_pin_and_does_not_jump_to_latest() {
    let home = tempfile::tempdir().expect("tempdir");
    let old_version = "rust-v0.129.0";
    let latest_version = "rust-v0.130.0";
    setup_pinned_old_release(home.path(), old_version, latest_version);

    // `use codex <old>` was a deliberate pin: a plain launch under auto-update
    // `on` must keep running the pinned version, not jump to the newer release.
    ovm(home.path(), "http://127.0.0.1:9")
        .arg("codex")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!("version={old_version}")));
}

#[test]
fn launch_auto_update_can_use_newer_installed_release_when_tracking_latest() {
    let home = tempfile::tempdir().expect("tempdir");
    let old_version = "rust-v0.129.0";
    let latest_version = "rust-v0.130.0";
    setup_pinned_old_release(home.path(), old_version, latest_version);

    // No pin file means "track latest" (also the state of installs that predate
    // pinning): auto-update may advance to the newer installed release.
    fs::remove_file(home.path().join(".ovm/products/codex/pinned")).expect("remove pin");

    ovm(home.path(), "http://127.0.0.1:9")
        .arg("codex")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "version={latest_version}"
        )));
}

#[test]
fn launch_latest_with_args_refreshes_latest_then_uses_installed_release() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";

    let script = "#!/bin/sh\necho \"args=$*\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .args(["codex", "latest", "--version"])
        .assert()
        .success()
        .stdout(predicates::str::contains("args=--version"));
}

#[test]
fn launch_latest_without_args_refreshes_latest_then_prompts_without_running_in_non_tty() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";

    let script = "#!/bin/sh\necho should-not-run\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);

    ovm(home.path(), &url)
        .args(["codex", "latest"])
        .assert()
        .success()
        .stderr(predicates::str::contains(format!(
            "Now using Codex {version}"
        )))
        .stdout(predicates::str::is_empty());
}

#[test]
fn launch_latest_without_network_uses_latest_installed_release() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";

    let script = "#!/bin/sh\necho \"args=$*\"\n";
    let (_server, url) = install_fake_codex(home.path(), version, script);
    let offline_server = Server::new();

    ovm(home.path(), &url)
        .env("OVM_REGISTRY_BASE_URL", offline_server.url())
        .env(
            "OVM_CODEX_RELEASES_URL",
            format!("{}/missing", offline_server.url()),
        )
        .args(["codex", "latest", "--version"])
        .assert()
        .success()
        .stderr(predicates::str::contains(
            "Could not reach update service; using latest installed Codex",
        ))
        .stdout(predicates::str::contains("args=--version"));
}

/// `ovm use claude <version>` must leave `~/.local/bin/claude` an OVM-owned
/// symlink targeting `~/.ovm/bin/claude` (the OVM-managed launcher), so Claude
/// Code's interactive startup probe (`native_check_install`) stops printing
/// "claude command at … missing or broken". That managed launcher is the ovm
/// binary itself (multi-call dispatch), so launches route through ovm and
/// auto-update while staying OVM-owned. Auto-maintenance half of the launcher
/// hygiene that runs without `ovm doctor claude --fix`.
#[test]
fn use_claude_creates_ovm_owned_local_bin_launcher() {
    use std::os::unix::fs::PermissionsExt;

    let home = tempfile::tempdir().expect("tempdir");
    let hp = home.path();
    let version = "2.1.170";

    // Seed a fake installed Claude version with an executable native binary.
    let native = hp
        .join(".ovm/products/claude/versions")
        .join(version)
        .join("native");
    fs::create_dir_all(&native).expect("mkdir version dir");
    let binary = native.join("claude");
    fs::write(&binary, "#!/bin/sh\necho claude\n").expect("write binary");
    fs::write(native.join(".complete"), "").expect("write completion marker");
    let mut perms = fs::metadata(&binary).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&binary, perms).expect("chmod");

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", hp)
        .env("NO_COLOR", "1")
        .args(["use", "claude", version])
        .assert()
        .success();

    let local_launcher = hp.join(".local/bin/claude");
    let managed_launcher = hp.join(".ovm/bin/claude");
    assert!(
        local_launcher.is_symlink(),
        "expected ~/.local/bin/claude to be an OVM-owned symlink"
    );
    // The probe launcher targets the OVM-managed launcher textually, which is
    // what keeps claude_install's launcher_state classifying it OvmOwned.
    assert_eq!(
        fs::read_link(&local_launcher).expect("read launcher"),
        managed_launcher,
        "~/.local/bin/claude must target the OVM-managed launcher"
    );
    // ~/.ovm/bin/claude is now the ovm binary itself (multi-call dispatch), so
    // the probe launcher resolves through it — not straight to the version
    // binary — and launches route through ovm for auto-update.
    assert_eq!(
        fs::canonicalize(&local_launcher).expect("resolve launcher"),
        fs::canonicalize(&managed_launcher).expect("resolve managed launcher"),
        "launcher must resolve through the OVM-managed launcher"
    );
}
