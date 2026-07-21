//! Integration tests for `ovm adopt <product> [path]`.
//!
//! `adopt` imports an EXISTING non-OVM install into OVM's management WITHOUT
//! deleting the original. The transaction is: discover/accept the foreign
//! binary → run it with `--version` → parse a semver → install that managed
//! version (download) → activate it → report PATH takeover. These tests drive
//! the real `ovm` binary via `assert_cmd` against an isolated HOME, using tiny
//! FAKE foreign binaries (shell scripts that print a version for `--version`)
//! and mockito servers impersonating the release sources — so nothing touches
//! the real `~/.ovm/` or the network.
//!
//! The core safety property — the original install is left on disk — is asserted
//! in every positive case and in the failure cases.

#![cfg(unix)]

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Server, ServerGuard};
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use tar::Builder;

/// Write an executable shell script that prints `version_line` for any args
/// (so `--version` yields it). This stands in for a foreign product install.
fn fake_binary(dir: &Path, name: &str, version_line: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("mkdir fake dir");
    let path = dir.join(name);
    fs::write(&path, format!("#!/bin/sh\necho '{version_line}'\n")).expect("write fake binary");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod fake binary");
    path
}

/// Build a gzipped tarball containing a single file (Codex release layout).
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

/// Build a Pi release bundle (`pi/pi` binary + `pi/package.json`).
fn make_pi_bundle(binary_contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = Builder::new(encoder);

        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(binary_contents.len() as u64);
        hdr.set_mode(0o755);
        hdr.set_cksum();
        builder
            .append_data(&mut hdr, "pi/pi", binary_contents)
            .expect("append pi");

        let pkg = br#"{"name":"pi","version":"0.67.6"}"#;
        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(pkg.len() as u64);
        hdr.set_mode(0o644);
        hdr.set_cksum();
        builder
            .append_data(&mut hdr, "pi/package.json", &pkg[..])
            .expect("append pkg");

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

fn expected_pi_asset() -> &'static str {
    #[cfg(all(target_os = "macos", target_arch = "aarch64"))]
    {
        "pi-darwin-arm64.tar.gz"
    }
    #[cfg(all(target_os = "macos", target_arch = "x86_64"))]
    {
        "pi-darwin-x64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "aarch64"))]
    {
        "pi-linux-arm64.tar.gz"
    }
    #[cfg(all(target_os = "linux", target_arch = "x86_64"))]
    {
        "pi-linux-x64.tar.gz"
    }
}

/// Mock the Codex GitHub Releases API for a single `<tag>` (e.g. `rust-v0.144.0`).
/// Only the endpoints `install` needs (`/tags/<tag>` + the asset) are mounted.
fn setup_codex_mock(tag: &str, binary_contents: &[u8]) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_codex_asset();
    let asset_body = make_tarball(expected_codex_entry(), binary_contents);

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/{tag}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json)
        .expect_at_least(1)
        .create();

    let base = server.url();
    (server, base)
}

/// Mock the Pi releases API for a single `<version>` (unprefixed, e.g. `0.67.6`).
fn setup_pi_mock(version: &str, binary_contents: &[u8]) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_pi_asset();
    let asset_body = make_pi_bundle(binary_contents);

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"v{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/v{version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json)
        .expect_at_least(1)
        .create();

    let base = server.url();
    (server, base)
}

/// A fresh `ovm` invocation isolated to `home`, wired to the Codex mock source.
fn codex_ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_CODEX_RELEASES_URL", releases_url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

/// A fresh `ovm` invocation isolated to `home`, wired to the Pi mock source.
fn pi_ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_PI_RELEASES_URL", releases_url)
        .env("OVM_PI_NPM_REGISTRY_URL", releases_url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

// ---------------------------------------------------------------------------
// Codex

#[test]
fn codex_adopt_by_explicit_path_imports_without_deleting_original() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    // Foreign binary reports "0.144.0" -> normalized tag "rust-v0.144.0".
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock(tag, b"#!/bin/sh\necho managed-codex\n");
    let binary = fake_binary(foreign.path(), "codex", "codex-cli 0.144.0 (rust-v0.144.0)");

    codex_ovm(home.path(), &releases_url)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("0.144.0"));

    // Core safety property: the original foreign binary is untouched.
    assert!(
        binary.exists(),
        "adopt must not delete the original install"
    );

    // The managed version is now installed and listed.
    codex_ovm(home.path(), &releases_url)
        .args(["ls", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));

    // It is usable: activate, then resolve via `which`.
    codex_ovm(home.path(), &releases_url)
        .args(["use", "codex", tag])
        .assert()
        .success();
    codex_ovm(home.path(), &releases_url)
        .args(["which", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));

    // adopt already activated it, so `current` reports the adopted version.
    codex_ovm(home.path(), &releases_url)
        .args(["current", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));
}

#[test]
fn codex_adopt_discovers_foreign_binary_on_path_and_skips_ovm_managed() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock(tag, b"#!/bin/sh\necho managed-codex\n");

    // An OVM-managed `codex` sits earlier on PATH — adopt must SKIP it (never
    // adopt our own binary) and fall through to the genuine foreign install.
    let ovm_bin = home.path().join(".ovm/bin");
    fs::create_dir_all(&ovm_bin).expect("mkdir ovm bin");
    fake_binary(&ovm_bin, "codex", "ovm-managed 9.9.9");
    let foreign_binary = fake_binary(foreign.path(), "codex", "codex-cli 0.144.0");

    // PATH: OVM-managed dir first, foreign dir second. No path arg -> discovery.
    let path_value = format!("{}:{}", ovm_bin.display(), foreign.path().display());
    codex_ovm(home.path(), &releases_url)
        .env("PATH", &path_value)
        .args(["adopt", "codex"])
        .assert()
        .success()
        // The discovered binary is the foreign one, not the ~/.ovm/bin one.
        .stdout(predicate::str::contains(foreign.path().to_str().unwrap()));

    assert!(
        foreign_binary.exists(),
        "adopt must not delete the discovered original"
    );

    codex_ovm(home.path(), &releases_url)
        .args(["ls", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));
}

#[test]
fn codex_adopt_rejects_unparseable_version() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    // No mock server: parsing fails before any install/download is attempted.
    let binary = fake_binary(foreign.path(), "codex", "version: unknown build");

    codex_ovm(home.path(), "http://127.0.0.1:1")
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Could not parse a version"));

    // Original survives, and nothing was imported.
    assert!(binary.exists(), "adopt must not delete the original");
    assert!(
        !home.path().join(".ovm/products/codex/versions").exists(),
        "a version-parse failure must import nothing"
    );
}

#[test]
fn adopt_missing_binary_path_errors() {
    let home = tempfile::tempdir().expect("tempdir");
    let missing = home.path().join("no-such-dir/codex");

    codex_ovm(home.path(), "http://127.0.0.1:1")
        .args(["adopt", "codex", missing.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("binary not found"));
}

#[test]
fn codex_adopt_is_idempotent() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock(tag, b"#!/bin/sh\necho managed-codex\n");
    let binary = fake_binary(foreign.path(), "codex", "codex-cli 0.144.0");

    // First adopt installs the managed version.
    codex_ovm(home.path(), &releases_url)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .success();

    // Second adopt of the same version is safe and reports it is already
    // installed rather than erroring on a duplicate install.
    codex_ovm(home.path(), &releases_url)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already installed"));

    assert!(binary.exists(), "adopt must not delete the original");

    // Still exactly one managed version present.
    let versions_dir = home.path().join(".ovm/products/codex/versions");
    let entries: Vec<_> = fs::read_dir(&versions_dir)
        .expect("list versions")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_type().map(|t| t.is_dir()).unwrap_or(false))
        .collect();
    assert_eq!(
        entries.len(),
        1,
        "re-adopt must not create a second version"
    );
}

// ---------------------------------------------------------------------------
// Pi

#[test]
fn pi_adopt_by_explicit_path_imports_without_deleting_original() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let version = "0.67.6";
    let (_server, releases_url) = setup_pi_mock(version, b"#!/bin/sh\necho managed-pi\n");
    let binary = fake_binary(foreign.path(), "pi", "pi 0.67.6");

    pi_ovm(home.path(), &releases_url)
        .args(["adopt", "pi", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains(version));

    assert!(
        binary.exists(),
        "adopt must not delete the original install"
    );

    pi_ovm(home.path(), &releases_url)
        .args(["ls", "pi"])
        .assert()
        .success()
        .stdout(predicate::str::contains(version));

    pi_ovm(home.path(), &releases_url)
        .args(["use", "pi", version])
        .assert()
        .success();
    pi_ovm(home.path(), &releases_url)
        .args(["which", "pi"])
        .assert()
        .success()
        .stdout(predicate::str::contains("release/bundle/pi/pi"));
}

// ---------------------------------------------------------------------------
// Claude
//
// Claude's official binaries come from a hardcoded GCS CDN with no test-server
// override, so a full download-based adopt is not hermetic. We instead pre-seed
// a COMPLETE managed native install so adopt takes the "already installed"
// branch — this still exercises the Claude-specific path (find/parse/version +
// activation + `maintain_claude_launcher`/`nudge_if_claude_install_drift`) and
// the original-survives guarantee, without touching the network.

/// Seed a complete managed Claude native install for `version` under `home`.
/// Mirrors the on-disk layout `install_is_complete` reads: a `native/claude`
/// binary plus a `native/.complete` marker.
fn seed_complete_claude_native(home: &Path, version: &str) {
    let native = home
        .join(".ovm/products/claude/versions")
        .join(version)
        .join("native");
    fs::create_dir_all(&native).expect("mkdir native");
    fs::write(native.join("claude"), b"#!/bin/sh\necho seeded-claude\n").expect("write claude bin");
    fs::set_permissions(native.join("claude"), fs::Permissions::from_mode(0o755))
        .expect("chmod claude bin");
    fs::write(native.join(".complete"), b"").expect("write complete marker");
}

fn claude_ovm(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

#[test]
fn claude_adopt_already_installed_version_activates_without_network() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let version = "2.1.91";
    seed_complete_claude_native(home.path(), version);
    let binary = fake_binary(foreign.path(), "claude", "2.1.91 (Claude Code)");

    claude_ovm(home.path())
        .args(["adopt", "claude", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("already installed"))
        .stdout(predicate::str::contains(version));

    // Original survives; Claude's launcher maintenance did not crash the run.
    assert!(
        binary.exists(),
        "adopt must not delete the original install"
    );

    claude_ovm(home.path())
        .args(["ls", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains(version));
    claude_ovm(home.path())
        .args(["which", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains(version));
    claude_ovm(home.path())
        .args(["current", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains(version));
}

#[test]
fn claude_adopt_rejects_unparseable_version() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let binary = fake_binary(foreign.path(), "claude", "Claude Code (build unknown)");

    claude_ovm(home.path())
        .args(["adopt", "claude", binary.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Could not parse a version"));

    assert!(binary.exists(), "adopt must not delete the original");
    assert!(
        !home.path().join(".ovm/products/claude/versions").exists(),
        "a version-parse failure must import nothing"
    );
}
