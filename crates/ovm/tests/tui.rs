//! TUI (interactive picker) tests via a pseudo-terminal.
//!
//! rexpect spawns `ovm select` under a real PTY so the dialoguer-style picker
//! actually reads key events. We verify the product picker navigation,
//! version picker navigation, and `esc` back-to-products behavior.

use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, ServerGuard};
use rexpect::spawn;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use tar::Builder;

fn make_tarball(entry_name: &str, contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = Builder::new(encoder);

        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(contents.len() as u64);
        hdr.set_mode(0o755);
        hdr.set_cksum();
        builder
            .append_data(&mut hdr, entry_name, contents)
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

/// Install result: (releases_server, registry_server, releases_url, registry_url).
struct MockEnv {
    _releases_server: ServerGuard,
    _registry_server: ServerGuard,
    releases_url: String,
    registry_url: String,
}

/// Mock the Codex releases API + registry, install one version into `home`.
fn install_codex_version(home: &Path, version: &str) -> MockEnv {
    let mut releases_server = mockito::Server::new();
    let mut registry_server = mockito::Server::new();

    let asset_name = expected_codex_asset();
    let entry = expected_codex_entry();
    let tarball = make_tarball(entry, b"#!/bin/sh\necho fake\n");

    releases_server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_body(tarball)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", releases_server.url());
    let release_json = format!(
        r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
    );
    releases_server
        .mock("GET", format!("/tags/{version}").as_str())
        .with_status(200)
        .with_body(release_json)
        .create();

    releases_server
        .mock("GET", "/")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_body(format!(r#"[{{"tag_name":"{version}","assets":[]}}]"#))
        .create();

    // Mock the mochiexists registry so the TUI finds our version there.
    registry_server
        .mock("GET", "/codex.json")
        .with_status(200)
        .with_body(format!(
            r#"{{"versions":[{{"version":"{version}","date":"2026-04-01"}}]}}"#
        ))
        .create();

    let releases_url = releases_server.url();
    let registry_url = registry_server.url();

    // Pre-install via non-interactive command.
    let ovm_bin = assert_cmd::cargo::cargo_bin("ovm");
    std::process::Command::new(&ovm_bin)
        .env("HOME", home)
        .env("OVM_CODEX_RELEASES_URL", &releases_url)
        .env("OVM_REGISTRY_BASE_URL", &registry_url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .args(["install", "codex", version])
        .status()
        .expect("spawn install");

    MockEnv {
        _releases_server: releases_server,
        _registry_server: registry_server,
        releases_url,
        registry_url,
    }
}

fn ovm_bin_path() -> PathBuf {
    assert_cmd::cargo::cargo_bin("ovm")
}

fn create_installed_codex_dir(home: &Path, version: &str) {
    fs::create_dir_all(
        home.join(".ovm")
            .join("products")
            .join("codex")
            .join("versions")
            .join(version),
    )
    .expect("create installed codex version dir");
}

/// Seed a *fresh* version-index cache for `product` so the picker renders its
/// history synchronously from the local cache and skips the background refresh.
/// Lets frame-snapshot tests stay deterministic now that the live picker folds
/// remote versions in asynchronously.
fn seed_version_index(home: &Path, product: &str, versions: &[(&str, &str)]) {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs();
    let version_list = versions
        .iter()
        .map(|(version, _)| format!("{version:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let dates = versions
        .iter()
        .map(|(version, date)| format!("{version:?}:{date:?}"))
        .collect::<Vec<_>>()
        .join(",");
    let json = format!(r#"{{"versions":[{version_list}],"dates":{{{dates}}},"fetched_at":{now}}}"#);
    let dir = home.join(".ovm").join("cache").join("registry");
    fs::create_dir_all(&dir).expect("create registry cache dir");
    fs::write(dir.join(format!("{product}.json")), json).expect("write version index");
}

fn visible_output(input: &str) -> String {
    let stripped = console::strip_ansi_codes(input);
    let mut out = String::new();
    for ch in stripped.chars() {
        if ch == '\r' {
            out.push('\n');
        } else if ch == '\u{8}' {
            out.pop();
        } else if !ch.is_control() || ch == '\n' || ch == '\t' {
            out.push(ch);
        }
    }
    out
}

#[test]
fn product_picker_esc_exits_cleanly() {
    let home = tempfile::tempdir().expect("tempdir");
    let bin = ovm_bin_path();

    // Spawn `ovm select` in a PTY. No product → opens product picker.
    let cmd = format!(
        "env HOME={} {} select",
        home.path().display(),
        bin.display()
    );
    let mut session = spawn(&cmd, Some(5_000)).expect("spawn");

    // Product picker header appears
    session
        .exp_string("Select a product")
        .expect("product header");

    // Press Esc — send a raw ESC byte (no newline) so dialoguer sees it as an
    // escape key event rather than ESC + Enter. `send` writes raw bytes;
    // `send_line` would append \n and defeat the test.
    session.send("\x1b").expect("send esc");
    session.flush().expect("flush esc");

    // Wait for process to finish cleanly after Esc.
    session.exp_eof().expect("clean exit on esc");
}

#[test]
fn version_picker_shows_installed_version() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let env = install_codex_version(home.path(), version);

    let bin = ovm_bin_path();
    let cmd = format!(
        "env HOME={} OVM_CODEX_RELEASES_URL={} OVM_REGISTRY_BASE_URL={} OVM_SKIP_SIGNATURE_VERIFY=1 {} select codex",
        home.path().display(),
        env.releases_url,
        env.registry_url,
        bin.display()
    );
    let mut session = spawn(&cmd, Some(10_000)).expect("spawn");

    // The installed version should appear in the picker
    session.exp_string(version).expect("version in picker");

    let _ = session.send_control('c');
    let _ = session.exp_eof();
}

#[test]
fn version_picker_shows_installed_instantly_when_registry_unreachable() {
    // The headline guarantee: with something installed but no cache and the
    // registry unreachable, the picker paints the installed version immediately
    // instead of blocking on the network. The background refresh fails quietly.
    let home = tempfile::tempdir().expect("tempdir");
    create_installed_codex_dir(home.path(), "rust-v0.135.0");

    // A URL at a closed port → connection refused (no internet).
    let dead = {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").expect("bind");
        let addr = listener.local_addr().expect("addr");
        drop(listener);
        format!("http://{addr}")
    };

    let bin = ovm_bin_path();
    let cmd = format!(
        "env HOME={} NO_COLOR=1 OVM_REGISTRY_BASE_URL={} OVM_CODEX_RELEASES_URL={} {} select codex",
        home.path().display(),
        dead,
        dead,
        bin.display(),
    );

    let start = Instant::now();
    let mut session = spawn(&cmd, Some(8_000)).expect("spawn");
    session
        .exp_string("rust-v0.135.0")
        .expect("installed version shown instantly");
    let elapsed = start.elapsed();
    assert!(
        elapsed <= Duration::from_secs(3),
        "picker blocked on the unreachable registry: {elapsed:?}"
    );

    let _ = session.send_control('c');
    let _ = session.exp_eof();
}

#[test]
fn codex_picker_defaults_to_real_releases_and_r_toggles_all_releases() {
    let home = tempfile::tempdir().expect("tempdir");
    create_installed_codex_dir(home.path(), "dev:local-build");
    create_installed_codex_dir(home.path(), "rust-v0.135.0");

    // Seed a fresh cache so the picker renders the full history synchronously
    // (no background refresh) — the live fold-in is exercised separately.
    seed_version_index(
        home.path(),
        "codex",
        &[
            ("rust-v0.138.0-alpha.1", "2026-06-02"),
            ("rust-v0.137.0", "2026-06-01"),
            ("rust-v0.136.0-alpha.2", "2026-05-30"),
            ("rust-v0.136.0", "2026-05-29"),
        ],
    );

    let registry_server = mockito::Server::new();
    let bin = ovm_bin_path();
    let cmd = format!(
        "env HOME={} NO_COLOR=1 OVM_REGISTRY_BASE_URL={} {} select codex",
        home.path().display(),
        registry_server.url(),
        bin.display(),
    );
    let mut session = spawn(&cmd, Some(10_000)).expect("spawn");

    let before_footer = session
        .exp_string("r all")
        .expect("real-release frame footer");
    let real_frame = visible_output(&format!("{before_footer}r all"));

    assert!(real_frame.contains("showing real releases"), "{real_frame}");
    assert!(
        real_frame.contains("dev      dev:local-build"),
        "{real_frame}"
    );
    assert!(
        real_frame.contains("release  rust-v0.135.0"),
        "{real_frame}"
    );
    assert!(
        real_frame.contains("release  rust-v0.137.0"),
        "{real_frame}"
    );
    assert!(
        real_frame.contains("release  rust-v0.136.0"),
        "{real_frame}"
    );
    assert!(
        !real_frame.contains("rust-v0.138.0-alpha.1"),
        "{real_frame}"
    );
    assert!(
        !real_frame.contains("rust-v0.136.0-alpha.2"),
        "{real_frame}"
    );
    assert!(!real_frame.contains("/pet"), "{real_frame}");
    assert!(!real_frame.contains("[real releases]"), "{real_frame}");

    let dev_index = real_frame
        .find("dev      dev:local-build")
        .expect("dev row");
    let release_index = real_frame
        .find("release  rust-v0.137.0")
        .expect("release row");
    assert!(
        dev_index < release_index,
        "dev rows should render above remote releases:\n{real_frame}"
    );

    session.send("r").expect("send r");
    session.flush().expect("flush r");
    let before_toggle_footer = session
        .exp_string("r real")
        .expect("all-release frame footer");
    let all_frame = visible_output(&format!("{before_toggle_footer}r real"));

    assert!(all_frame.contains("showing all releases"), "{all_frame}");
    assert!(all_frame.contains("rust-v0.138.0-alpha.1"), "{all_frame}");
    assert!(all_frame.contains("rust-v0.136.0-alpha.2"), "{all_frame}");

    let _ = session.send_control('c');
    let _ = session.exp_eof();
}

#[test]
fn version_picker_loads_quickly_for_all_products_from_registry() {
    let mut registry_server = mockito::Server::new();
    let products = [
        ("claude", "2.1.112"),
        ("codex", "rust-v0.130.0"),
        ("pi", "0.67.6"),
    ];

    for (product, version) in products {
        registry_server
            .mock("GET", format!("/{product}.json").as_str())
            .with_status(200)
            .with_body(format!(
                r#"{{"versions":[{{"version":"{version}","date":"2026-05-13"}}]}}"#
            ))
            .create();
    }

    let bin = ovm_bin_path();
    let max_elapsed = Duration::from_secs(3);

    for (product, version) in products {
        let home = tempfile::tempdir().expect("tempdir");
        let cmd = format!(
            "env HOME={} OVM_REGISTRY_BASE_URL={} {} select {}",
            home.path().display(),
            registry_server.url(),
            bin.display(),
            product
        );

        let start = Instant::now();
        let mut session = spawn(&cmd, Some(5_000)).expect("spawn");
        session.exp_string(version).expect("version in picker");
        let elapsed = start.elapsed();

        let _ = session.send_control('c');
        let _ = session.exp_eof();

        assert!(
            elapsed <= max_elapsed,
            "{product} version picker took {elapsed:?}, expected <= {max_elapsed:?}"
        );
    }
}

#[test]
fn product_picker_lists_ovm_when_alpha_channel_is_set() {
    // The gate tie-in: opting into the alpha self-update channel surfaces OVM
    // as a selectable entry in the product picker.
    let home = tempfile::tempdir().expect("tempdir");
    fs::create_dir_all(home.path().join(".ovm")).expect("mkdir");
    fs::write(
        home.path().join(".ovm/config.json"),
        r#"{"self":{"channel":"alpha"}}"#,
    )
    .expect("write config");

    let bin = ovm_bin_path();
    let cmd = format!(
        "env HOME={} NO_COLOR=1 {} select",
        home.path().display(),
        bin.display()
    );
    let mut session = spawn(&cmd, Some(5_000)).expect("spawn");

    session
        .exp_string("Select a product")
        .expect("product header");
    session
        .exp_string("Open Version Manager")
        .expect("ovm entry listed on alpha channel");

    session.send("\x1b").expect("send esc");
    session.flush().expect("flush esc");
    let _ = session.exp_eof();
}

#[test]
fn select_with_direct_version_switches_non_interactively() {
    // `ovm select codex <version>` with an installed version should switch directly
    // without opening the TUI.
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let env = install_codex_version(home.path(), version);

    assert_cmd::Command::cargo_bin("ovm")
        .expect("bin")
        .env("HOME", home.path())
        .env("OVM_CODEX_RELEASES_URL", &env.releases_url)
        .env("OVM_REGISTRY_BASE_URL", &env.registry_url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .args(["select", "codex", version])
        .assert()
        .success()
        .stderr(predicates::str::contains("Now using"));
}
