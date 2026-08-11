//! Integration tests for `ovm adopt <product> [path]`.
//!
//! `adopt` imports an EXISTING non-OVM install into OVM's management WITHOUT
//! deleting the original. The transaction is: discover/accept the foreign
//! binary → run it with `--version` → parse a semver → make that version a
//! managed install (by copying the user's own binary when it is a
//! self-contained executable, otherwise by downloading that same version) →
//! activate it → report PATH takeover. A copied binary is proven before it is
//! published: the staged copy must carry the publisher's signature and must
//! still report the version it is being installed as. These tests drive
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
use mockito::{Matcher, Server, ServerGuard};
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
    let asset_size = asset_body.len();

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}","size":{asset_size}}}]}}"#,
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
    let asset_size = asset_body.len();

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"v{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}","size":{asset_size}}}]}}"#,
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

/// The repro that used to delete the user's file.
///
/// `ovm adopt codex <path>` with a path inside the store reached the import
/// transaction, which quarantines and REMOVES that version's source tree before
/// the copy runs. An install that died half-way — binary present, no
/// `.complete` — was therefore deleted out from under the very file being
/// adopted: the copy failed with ENOENT and the command that promises "Original
/// install left untouched" had left nothing at all. It must refuse instead, name
/// the command that actually fixes this, and touch nothing. Every upstream is
/// pointed at a dead port, so a refusal cannot be a quiet download either.
#[test]
fn codex_adopt_refuses_a_path_inside_the_ovm_store_and_keeps_it() {
    let home = tempfile::tempdir().expect("tempdir");
    // A self-contained executable inside the incomplete managed tree of the
    // version it reports: the exact shape that took the import branch, whose
    // transaction removes `release/` before the copy runs. Verified against the
    // unfixed code — the file was deleted and the run died on a bare ENOENT.
    let tag = format!("rust-v{}", ovm_binary_version());
    let managed_bin_dir = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(&tag)
        .join("release/bin");
    let binary = self_contained_binary(&managed_bin_dir, "codex-real");
    let bytes_before = fs::read(&binary).expect("read managed binary");
    // No `.complete` marker anywhere: this is the incomplete managed tree.

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already inside OVM's store"))
        .stderr(predicate::str::contains("ovm install codex <version>"))
        .stderr(predicate::str::contains("ovm use codex <version>"));

    assert!(
        binary.exists(),
        "adopt deleted the file it was pointed at: {}",
        binary.display()
    );
    assert_eq!(
        fs::read(&binary).expect("read managed binary"),
        bytes_before,
        "adopt rewrote the file it was pointed at"
    );
}

/// Same refusal through a symlink from outside the store: the check resolves
/// both sides, so a link is not a way to smuggle a managed path past it.
#[test]
fn codex_adopt_refuses_a_symlink_that_points_into_the_ovm_store() {
    let home = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside dir");
    let tag = "rust-v0.144.0";
    let managed_bin_dir = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(tag)
        .join("release/bin");
    let binary = fake_binary(
        &managed_bin_dir,
        "codex",
        "codex-cli 0.144.0 (rust-v0.144.0)",
    );
    let link = outside.path().join("codex");
    std::os::unix::fs::symlink(&binary, &link).expect("symlink into the store");

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["adopt", "codex", link.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("already inside OVM's store"));

    assert!(binary.exists(), "adopt deleted the symlink's target");
}

/// Both refusals above judge a *name*, and a name can be true and useless at
/// once. A hard link outside the store is a second, entirely honest name for a
/// file inside it: nothing to resolve, nothing to see. (The same divergence is
/// reachable by racing an intermediate directory of the path against the
/// canonicalize/open window — the hard link is that end state, made
/// deterministic.) The import must be refused anyway, because the file the open
/// handle holds is one the transaction deletes before it copies. Verified
/// against the unfixed code: the adopt reported success and `release/meta.json`
/// was gone.
#[test]
fn codex_adopt_refuses_a_hard_link_to_a_file_the_install_would_delete() {
    let home = tempfile::tempdir().expect("tempdir");
    let outside = tempfile::tempdir().expect("outside dir");
    let tag = format!("rust-v{}", ovm_binary_version());
    let managed_root = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(&tag)
        .join("release");
    // Incomplete managed tree for the version the binary reports: no
    // `.complete`, and more in it than the binary — which is the part a copy
    // could never put back.
    let binary = self_contained_binary(&managed_root.join("bin"), "codex-real");
    let rest_of_the_tree = managed_root.join("meta.json");
    fs::write(&rest_of_the_tree, "{}").expect("write the rest of the tree");

    let link = outside.path().join("codex");
    fs::hard_link(&binary, &link).expect("hard link the managed binary out of the store");

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["adopt", "codex", link.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(predicate::str::contains("the same file as"))
        .stderr(predicate::str::contains(
            "removes before it copies anything",
        ))
        .stderr(predicate::str::contains(format!("ovm install codex {tag}")));

    assert!(link.exists(), "a refusal must delete nothing");
    assert!(binary.exists(), "adopt deleted the file it was pointed at");
    assert!(
        rest_of_the_tree.exists(),
        "adopt deleted the rest of the incomplete managed tree at {}",
        rest_of_the_tree.display()
    );
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

/// Like `setup_codex_mock`, but also answers "what is latest?" so the
/// first-launch install path can be exercised without the network.
fn setup_codex_mock_with_latest(tag: &str, binary_contents: &[u8]) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_codex_asset();
    let asset_body = make_tarball(expected_codex_entry(), binary_contents);
    let asset_size = asset_body.len();

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{tag}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}","size":{asset_size}}}]}}"#,
    );
    for path in [format!("/tags/{tag}"), "/latest".to_string()] {
        server
            .mock("GET", path.as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(release_json.clone())
            .create();
    }
    server
        .mock("GET", "/")
        .match_query(Matcher::Any)
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{release_json}]"))
        .create();

    let base = server.url();
    (server, base)
}

// ---------------------------------------------------------------------------
// First-launch bootstrap.
//
// A brand-new machine used to dead-end: `ovm cx` said "Run: `ovm use <product>
// <version>`", and that command could not work either because nothing was
// installed — and it needed a version number the user had no way to know.
// Meanwhile a perfectly good unmanaged install often already sat on PATH,
// ignored, so `autoupdate on` was a promise over an empty set.
//
// Launching is a request to run the product. These tests assert it does.
// ---------------------------------------------------------------------------

/// Pin the bootstrap tests to the bootstrap. Auto-update is on by default, so
/// without this a launch would immediately chase the real registry and the test
/// would measure the update path instead of the first-launch decision.
fn disable_auto_update(home: &Path) {
    fs::create_dir_all(home.join(".ovm")).expect("mkdir .ovm");
    fs::write(
        home.join(".ovm/config.json"),
        r#"{"checkForUpdates": false, "autoUpdate": {"default": "off"}}"#,
    )
    .expect("write config");
}

/// A launcher that reports which managed binary actually ran, so the tests can
/// prove a real version was selected rather than an error printed.
const MANAGED_MARKER: &[u8] = b"#!/bin/sh\necho managed-codex-ran\n";

#[test]
fn first_launch_adopts_the_install_already_on_the_machine() {
    let home = tempfile::tempdir().expect("tempdir");
    disable_auto_update(home.path());
    let foreign = tempfile::tempdir().expect("foreign dir");
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock(tag, MANAGED_MARKER);
    let binary = fake_binary(foreign.path(), "codex", "codex-cli 0.144.0 (rust-v0.144.0)");

    let mut path = std::ffi::OsString::from(foreign.path());
    path.push(":/usr/bin:/bin");

    // No versions, no active version — just a launch.
    codex_ovm(home.path(), &releases_url)
        .env("PATH", &path)
        .args(["cx"])
        .assert()
        .success()
        .stdout(predicate::str::contains("managed-codex-ran"));

    // It adopted rather than downloading something newer over the top, and the
    // user's own binary is untouched.
    assert!(binary.exists(), "adoption must not delete the original");
    codex_ovm(home.path(), &releases_url)
        .args(["current", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));
}

#[test]
fn first_launch_installs_latest_when_the_machine_has_nothing() {
    let home = tempfile::tempdir().expect("tempdir");
    disable_auto_update(home.path());
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock_with_latest(tag, MANAGED_MARKER);

    // PATH deliberately holds no codex at all: nothing to adopt.
    // Every OTHER "latest" authority is dead-ended (the alpha.10 lesson:
    // codex-latest tests must dead-end npm AND the registry), so resolution
    // can only land on the releases mock. Without the npm dead-end this test
    // escaped to the real npm registry and installed a real Codex — that was
    // the harness gap that kept it `#[ignore]`d.
    codex_ovm(home.path(), &releases_url)
        .env("OVM_CODEX_NPM_REGISTRY_URL", OFFLINE_URL)
        .env("OVM_NPM_PACKAGE_URL", OFFLINE_URL)
        .env("OVM_REGISTRY_BASE_URL", OFFLINE_URL)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("PATH", "/usr/bin:/bin")
        .args(["cx"])
        .assert()
        .success()
        .stdout(predicate::str::contains("managed-codex-ran"));

    codex_ovm(home.path(), &releases_url)
        .args(["current", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));
}

/// A URL nobody is listening on: every upstream call fails immediately
/// (ECONNREFUSED), which is what an offline machine looks like.
const OFFLINE_URL: &str = "http://127.0.0.1:1";

/// Point every upstream at a dead port so the invocation cannot reach the
/// network, and keep the detached refresh from doing so behind our back.
fn offline(cmd: &mut Command) -> &mut Command {
    cmd.env("OVM_CODEX_RELEASES_URL", OFFLINE_URL)
        .env("OVM_CODEX_NPM_REGISTRY_URL", OFFLINE_URL)
        .env("OVM_NPM_PACKAGE_URL", OFFLINE_URL)
        .env("OVM_REGISTRY_BASE_URL", OFFLINE_URL)
        .env("OVM_GITHUB_API_URL", OFFLINE_URL)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
}

/// A real, self-contained executable that prints a semver for `--version`: the
/// `ovm` test binary itself. Copied under `name` — never the product's own
/// binary name, or running it would re-enter OVM's launcher dispatch.
fn self_contained_binary(dir: &Path, name: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("mkdir foreign dir");
    let source = assert_cmd::cargo::cargo_bin("ovm");
    let path = dir.join(name);
    fs::copy(&source, &path).expect("copy self-contained binary");
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).expect("chmod");
    path
}

/// Adoption's promise is that the version already on the machine is preserved —
/// so when that install is a single self-contained executable, OVM copies it
/// into the version store instead of downloading a byte-identical copy. Proven
/// with every upstream pointed at a dead port: if anything is fetched, this
/// fails.
#[test]
fn codex_adopt_imports_a_self_contained_local_binary_without_downloading() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let binary = self_contained_binary(foreign.path(), "codex-real");
    let reported = ovm_binary_version();
    let tag = format!("rust-v{reported}");

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported the local binary"))
        .stdout(predicate::str::contains("nothing downloaded"));

    // The original is untouched and the managed install IS their binary.
    assert!(binary.exists(), "adopt must not delete the original");
    let managed = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(&tag)
        .join("release/bin/codex");
    assert_eq!(
        fs::read(&managed).expect("managed binary"),
        fs::read(&binary).expect("foreign binary"),
        "the managed install must be a copy of the adopted binary"
    );

    // And it is a first-class managed version: listed, active, resolvable.
    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["current", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(&tag));
}

/// The ordinary shim: `/opt/homebrew/bin/codex` → `…/Cellar/…/codex`. The
/// import opens the *resolved* file and refuses to follow a link at that final
/// step — which must not make a legitimate symlinked install unimportable. The
/// managed copy is the target's bytes, and both the link and its target survive.
#[test]
fn codex_adopt_imports_through_a_symlink_shim_without_downloading() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let target = self_contained_binary(&foreign.path().join("cellar"), "codex-real");
    let link = foreign.path().join("codex-shim");
    std::os::unix::fs::symlink(&target, &link).expect("symlink to the real binary");
    let tag = format!("rust-v{}", ovm_binary_version());

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .args(["adopt", "codex", link.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("Imported the local binary"))
        .stdout(predicate::str::contains("nothing downloaded"));

    assert!(link.exists(), "adopt must not delete the shim");
    assert!(target.exists(), "adopt must not delete the shim's target");
    let managed = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(&tag)
        .join("release/bin/codex");
    assert_eq!(
        fs::read(&managed).expect("managed binary"),
        fs::read(&target).expect("target binary"),
        "the managed install must be a copy of the file the shim points at"
    );
}

/// Downloaded Claude and Codex binaries are checked against the publisher's
/// Apple team ID before they are installed. An imported one skipped that
/// entirely, which made `ovm adopt` a way to get bytes nobody vouched for into
/// the version store under a real release's name.
///
/// Every other test in this file turns verification off, because its fixtures
/// are unsigned — so this one turns it back ON and drives the real CLI. The
/// `ovm` binary is a genuine self-contained executable, but it is ad-hoc
/// (linker) signed with no team identifier, which is exactly what a
/// substituted or rebuilt Codex looks like to `codesign`. It must be refused,
/// and nothing may be left in the store.
///
/// macOS only: the check is a no-op elsewhere, for imports exactly as for
/// downloads.
#[cfg(target_os = "macos")]
#[test]
fn codex_adopt_refuses_to_import_a_binary_the_publisher_did_not_sign() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let binary = self_contained_binary(foreign.path(), "codex-real");
    let tag = format!("rust-v{}", ovm_binary_version());

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .env_remove("OVM_SKIP_SIGNATURE_VERIFY")
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .failure()
        .stderr(
            // Adhoc-signed with no team is the likely verdict; a toolchain that
            // stops signing at all fails the earlier `codesign --verify` step.
            predicate::str::contains("code-sign").or(predicate::str::contains(
                "code signature verification failed",
            )),
        );

    assert!(binary.exists(), "adopt must not delete the original");
    let managed = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(&tag)
        .join("release/bin/codex");
    assert!(
        !managed.exists(),
        "an unverified binary was published at {}",
        managed.display()
    );
    assert!(
        !home
            .path()
            .join(".ovm/products/codex/versions")
            .join(&tag)
            .exists(),
        "a refused import must not leave a version directory behind"
    );
}

/// The version `ovm --version` reports, which is what adopt parses out of the
/// binary copied by `self_contained_binary`.
fn ovm_binary_version() -> String {
    let output = Command::cargo_bin("ovm")
        .expect("binary built")
        .arg("--version")
        .output()
        .expect("run ovm --version");
    let text = String::from_utf8_lossy(&output.stdout).into_owned();
    text.split_whitespace()
        .find(|token| token.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .expect("a version in `ovm --version`")
        .to_string()
}

/// A wrapper script (npm/Homebrew shim) must NEVER be imported: it is a few
/// lines pointing into a package tree OVM is not copying, so the copy would
/// break as soon as the user removes the original — which adopt then invites
/// them to do. Those adoptions download the matching managed build instead.
#[test]
fn codex_adopt_downloads_rather_than_importing_a_wrapper_script() {
    let home = tempfile::tempdir().expect("tempdir");
    let foreign = tempfile::tempdir().expect("foreign dir");
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock(tag, b"#!/bin/sh\necho managed-codex\n");
    let binary = fake_binary(foreign.path(), "codex", "codex-cli 0.144.0 (rust-v0.144.0)");

    codex_ovm(home.path(), &releases_url)
        .args(["adopt", "codex", binary.to_str().unwrap()])
        .assert()
        .success()
        .stdout(predicate::str::contains("wrapper script"))
        .stdout(predicate::str::contains("downloading managed"));

    // The managed binary came from upstream, not from the shim.
    let managed = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(tag)
        .join("release/bin/codex");
    assert_eq!(
        fs::read(&managed).expect("managed binary"),
        b"#!/bin/sh\necho managed-codex\n",
        "a shim must not be copied into the version store"
    );
}

/// The offline first launch: a machine with a working `codex` on PATH, no
/// managed install, and no network. Adoption cannot import a shim and cannot
/// download, and "install the latest release" cannot run either — so the launch
/// runs the binary the user already has instead of dying with the tool sitting
/// right there on PATH.
#[test]
fn first_launch_runs_the_unmanaged_binary_when_nothing_can_be_installed() {
    let home = tempfile::tempdir().expect("tempdir");
    disable_auto_update(home.path());
    let foreign = tempfile::tempdir().expect("foreign dir");
    // A shim: not importable, so every route to a managed install needs the
    // network that this test denies.
    let binary = fake_binary(
        foreign.path(),
        "codex",
        "codex-cli 0.144.0 (foreign-codex-ran)",
    );

    let mut path = std::ffi::OsString::from(foreign.path());
    path.push(":/usr/bin:/bin");

    let mut cmd = codex_ovm(home.path(), OFFLINE_URL);
    offline(&mut cmd)
        .env("PATH", &path)
        .args(["cx"])
        .assert()
        .success()
        .stdout(predicate::str::contains("foreign-codex-ran"))
        .stderr(predicate::str::contains("Launching the existing unmanaged"));

    assert!(binary.exists(), "the fallback must not delete the original");
    // Nothing was adopted or activated, so a later launch retries the managed
    // path once the network is back.
    assert!(
        !home.path().join(".ovm/products/codex/current").exists(),
        "a failed bootstrap must not leave a half-selected product"
    );
}

#[test]
fn first_launch_selects_an_installed_version_that_was_never_activated() {
    let home = tempfile::tempdir().expect("tempdir");
    disable_auto_update(home.path());
    let tag = "rust-v0.144.0";
    let (_server, releases_url) = setup_codex_mock_with_latest(tag, MANAGED_MARKER);

    // Install without switching — `ovm install` deliberately does not activate.
    codex_ovm(home.path(), &releases_url)
        .args(["install", "codex", tag])
        .assert()
        .success();
    // Launching must pick the installed version rather than erroring.
    codex_ovm(home.path(), &releases_url)
        .env("PATH", "/usr/bin:/bin")
        .args(["cx"])
        .assert()
        .success()
        .stdout(predicate::str::contains("managed-codex-ran"));

    codex_ovm(home.path(), &releases_url)
        .args(["which", "codex"])
        .assert()
        .success()
        .stdout(predicate::str::contains(tag));
}
