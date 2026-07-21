//! Full install/use/uninstall lifecycle test for Codex (the simplest source).
//!
//! Sets up mockito servers impersonating the GitHub Releases API and the asset
//! download endpoint, then drives `ovm install / use / ls / which / uninstall`
//! via `assert_cmd`. Verifies the filesystem ends up in the expected state at
//! each step and finishes restored to empty.
//!
//! This is the test that proves the whole pipeline — HTTP metadata fetch, asset
//! download, checksum, extraction, symlink switching, metadata persistence —
//! works without hitting real external services.

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, Server, ServerGuard};
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{mpsc, Arc};
use std::thread;
use std::time::Duration;
use tar::Builder;

/// Build a gzipped tarball containing a single file.
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

/// Spin up an `ovm` invocation isolated to `home`.
fn ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_CODEX_RELEASES_URL", releases_url)
        // Test fixtures are unsigned fake binaries; skip codesign verification.
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

/// Expected asset name for the current host target (matches what sources/codex.rs builds).
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

/// Set up a mockito server impersonating the Codex GitHub Releases API for a
/// single version, and return (server, base_url_for_releases).
fn setup_codex_mock(version: &str, binary_contents: &[u8]) -> (ServerGuard, String) {
    let mut server = Server::new();

    let asset_name = expected_codex_asset();
    let asset_entry = expected_codex_entry();
    let asset_body = make_tarball(asset_entry, binary_contents);

    // /assets/<asset_name> serves the tarball bytes
    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .create();

    // /tags/<version> serves release metadata that references our mock asset URL
    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/{version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json)
        .create();

    // /?per_page=100&page=1 returns the version list (for ls --remote)
    let list_json = format!(
        r#"[{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}]"#,
    );
    server
        .mock("GET", "/")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("per_page".into(), "100".into()),
            Matcher::UrlEncoded("page".into(), "1".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(list_json)
        .create();

    // Empty second page so pagination terminates
    server
        .mock("GET", "/")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("per_page".into(), "100".into()),
            Matcher::UrlEncoded("page".into(), "2".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();

    let base = server.url();
    (server, base)
}

#[test]
fn codex_full_install_use_uninstall_lifecycle() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let binary_contents = b"#!/bin/sh\necho fake-codex\n";

    let (_server, releases_url) = setup_codex_mock(version, binary_contents);

    // 1. Fresh home — no codex versions installed
    let codex_versions_dir: PathBuf = home.path().join(".ovm/products/codex/versions");
    assert!(!codex_versions_dir.exists());

    // 2. `ovm install codex <version>` — downloads, extracts, persists metadata
    ovm(home.path(), &releases_url)
        .args(["install", "codex", version])
        .assert()
        .success();

    let installed_bin = codex_versions_dir.join(version).join("release/bin/codex");
    assert!(installed_bin.exists(), "binary should be installed");
    assert_eq!(
        fs::read(&installed_bin).expect("read binary"),
        binary_contents,
        "binary contents should match what the mock served"
    );

    let meta_path = codex_versions_dir.join(version).join("release/meta.json");
    assert!(meta_path.exists(), "release metadata should be written");
    let meta: serde_json::Value =
        serde_json::from_slice(&fs::read(&meta_path).expect("read meta")).expect("parse meta");
    assert_eq!(meta["version"], version);
    assert_eq!(
        meta["assetName"],
        expected_codex_asset(),
        "assetName should match platform asset"
    );
    assert!(
        meta["archiveSha256"]
            .as_str()
            .map(|s| s.len() == 64)
            .unwrap_or(false),
        "sha256 should be 64 hex chars"
    );

    // 3. `ovm ls codex` — installed version appears
    ovm(home.path(), &releases_url)
        .args(["ls", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains(version));

    // 4. `ovm use codex <version>` — activates
    ovm(home.path(), &releases_url)
        .args(["use", "codex", version])
        .assert()
        .success();

    let bin_link = home.path().join(".ovm/bin/codex");
    assert!(
        bin_link.exists() || bin_link.is_symlink(),
        "active bin symlink should exist"
    );

    // 5. `ovm current codex` — prints the version
    ovm(home.path(), &releases_url)
        .args(["current", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains(version));

    // 6. `ovm which codex` — prints the binary path
    ovm(home.path(), &releases_url)
        .args(["which", "codex"])
        .assert()
        .success();

    // 7. `ovm stats` — includes codex in the dashboard
    ovm(home.path(), &releases_url)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicates::str::contains("Codex"))
        .stdout(predicates::str::contains("installed: 1"));

    // 8. Can't uninstall the active version — should error
    ovm(home.path(), &releases_url)
        .args(["uninstall", "codex", version])
        .assert()
        .failure()
        .stderr(predicates::str::contains("active"));

    // We'd need a second version to switch to before uninstalling; skip
    // the actual uninstall here and manually tear down.

    // 9. Clean — removes cached raw artifacts but keeps installed
    ovm(home.path(), &releases_url)
        .args(["clean", "codex", version])
        .assert()
        .success();
    assert!(installed_bin.exists(), "binary should survive clean");

    // 10. Explicit cleanup: delete the install dir to simulate a fresh
    //     state. (We can't test `uninstall` here because the version is active
    //     and there's no fallback to switch to first — that'd need a second
    //     mocked version. Covered separately in unit tests.)
    fs::remove_dir_all(codex_versions_dir.join(version)).expect("cleanup");
    assert!(!codex_versions_dir.join(version).exists());
}

/// Set up a mock server for a release that also publishes the
/// `codex-code-mode-host` sidecar asset (Codex 0.144.0+ spawns it from the
/// same directory as the main binary for every shell command).
fn setup_codex_mock_with_sidecar(
    version: &str,
    binary_contents: &[u8],
    sidecar_contents: &[u8],
) -> (ServerGuard, String) {
    let mut server = Server::new();

    let asset_name = expected_codex_asset();
    let asset_body = make_tarball(expected_codex_entry(), binary_contents);
    let triple = expected_codex_entry()
        .strip_prefix("codex-")
        .expect("entry is codex-<triple>");
    let sidecar_entry = format!("codex-code-mode-host-{triple}");
    let sidecar_asset_name = format!("{sidecar_entry}.tar.gz");
    let sidecar_body = make_tarball(&sidecar_entry, sidecar_contents);

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .create();
    server
        .mock("GET", format!("/assets/{sidecar_asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(sidecar_body)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let sidecar_url = format!("{}/assets/{sidecar_asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"{version}","assets":[
            {{"name":"{asset_name}","browser_download_url":"{asset_url}"}},
            {{"name":"{sidecar_asset_name}","browser_download_url":"{sidecar_url}"}}
        ]}}"#,
    );
    server
        .mock("GET", format!("/tags/{version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json)
        .create();

    let base = server.url();
    (server, base)
}

#[test]
fn codex_install_also_installs_code_mode_host_sidecar() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.144.0";
    let binary_contents = b"fake-codex-binary";
    let sidecar_contents = b"fake-host-binary";

    let (_server, releases_url) =
        setup_codex_mock_with_sidecar(version, binary_contents, sidecar_contents);

    ovm(home.path(), &releases_url)
        .args(["install", "codex", version])
        .assert()
        .success();

    let bin_dir = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(version)
        .join("release/bin");
    assert_eq!(
        fs::read(bin_dir.join("codex")).expect("read main binary"),
        binary_contents
    );
    assert_eq!(
        fs::read(bin_dir.join("codex-code-mode-host")).expect("read sidecar binary"),
        sidecar_contents,
        "code-mode host sidecar should be installed next to the main binary"
    );
    let leftovers: Vec<_> = fs::read_dir(&bin_dir)
        .expect("list bin dir")
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tar.gz") || name.ends_with(".tgz"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "no archives left behind: {leftovers:?}"
    );
}

#[test]
fn codex_ls_remote_hits_mock_registry() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let binary_contents = b"fake";
    let (_server, releases_url) = setup_codex_mock(version, binary_contents);

    ovm(home.path(), &releases_url)
        .args(["ls", "codex", "--remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains(version));
}

#[test]
fn install_rejects_duplicate_version() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let binary_contents = b"fake";
    let (_server, releases_url) = setup_codex_mock(version, binary_contents);

    ovm(home.path(), &releases_url)
        .args(["install", "codex", version])
        .assert()
        .success();

    // Second install of the same version should error
    ovm(home.path(), &releases_url)
        .args(["install", "codex", version])
        .assert()
        .failure()
        .stderr(predicates::str::contains("already installed"));
}

#[test]
fn concurrent_codex_install_waits_and_reuses_single_download() {
    use assert_cmd::cargo::CommandCargoExt;

    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.130.0";
    let asset_name = expected_codex_asset();
    let asset_body = make_tarball(expected_codex_entry(), b"single-flight-codex");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind server");
    let address = listener.local_addr().expect("server address");
    let base_url = format!("http://{address}");
    let asset_url = format!("{base_url}/assets/{asset_name}");
    let release_body = format!(
        r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#
    );
    let asset_requests = Arc::new(AtomicUsize::new(0));
    let asset_requests_for_server = Arc::clone(&asset_requests);
    let (asset_started_tx, asset_started_rx) = mpsc::channel();
    let (release_tx, release_rx) = mpsc::channel();

    let server = thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept request");
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                let read = stream.read(&mut buffer).expect("read request");
                if read == 0 {
                    break;
                }
                request.extend_from_slice(&buffer[..read]);
            }
            let request = String::from_utf8_lossy(&request);

            if request.starts_with(&format!("GET /tags/{version} ")) {
                write!(
                    stream,
                    "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                    release_body.len(),
                    release_body
                )
                .expect("write release response");
                continue;
            }

            assert!(
                request.starts_with(&format!("GET /assets/{asset_name} ")),
                "unexpected request: {request}"
            );
            asset_requests_for_server.fetch_add(1, Ordering::SeqCst);
            write!(
                stream,
                "HTTP/1.1 200 OK\r\nContent-Type: application/octet-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                asset_body.len()
            )
            .expect("write asset headers");
            let split = asset_body.len() / 2;
            stream
                .write_all(&asset_body[..split])
                .expect("write first chunk");
            stream.flush().expect("flush first chunk");
            asset_started_tx.send(()).expect("signal asset started");
            release_rx.recv().expect("release asset response");
            stream
                .write_all(&asset_body[split..])
                .expect("write final chunk");
        }
    });

    let make_command = || {
        let mut command = std::process::Command::cargo_bin("ovm").expect("binary built");
        command
            .env("HOME", home.path())
            .env("OVM_CODEX_RELEASES_URL", &base_url)
            .env("OVM_CODEX_NPM_REGISTRY_URL", "http://127.0.0.1:9")
            .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
            .env_remove("OVM_VERSION")
            .env_remove("OVM_PRODUCT")
            .args(["install", "codex", version])
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        command
    };

    let owner = make_command().spawn().expect("spawn owner");
    asset_started_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("owner reached delayed asset");
    let mut contender = make_command().spawn().expect("spawn contender");
    let contender_stderr = contender.stderr.take().expect("contender stderr");
    let (waiting_tx, waiting_rx) = mpsc::channel();
    let stderr_reader = thread::spawn(move || {
        let mut reader = BufReader::new(contender_stderr);
        let mut output = String::new();
        reader.read_line(&mut output).expect("read waiting line");
        waiting_tx
            .send(output.clone())
            .expect("signal contender waiting");
        reader
            .read_to_string(&mut output)
            .expect("read remaining contender stderr");
        output
    });
    let waiting_line = waiting_rx
        .recv_timeout(Duration::from_secs(5))
        .expect("contender reported waiting");
    assert!(waiting_line.contains("Waiting for another OVM process"));
    release_tx.send(()).expect("release download");

    let owner_output = owner.wait_with_output().expect("owner output");
    let contender_output = contender.wait_with_output().expect("contender output");
    let contender_stderr = stderr_reader.join().expect("stderr reader");
    server.join().expect("server thread");

    assert!(
        owner_output.status.success(),
        "owner failed: {}",
        String::from_utf8_lossy(&owner_output.stderr)
    );
    assert!(
        contender_output.status.success(),
        "contender failed: {}",
        contender_stderr
    );
    assert!(contender_stderr.contains("Waiting for another OVM process"));
    assert!(contender_stderr.contains("Reused Codex"));
    assert_eq!(asset_requests.load(Ordering::SeqCst), 1);

    let release_root = home
        .path()
        .join(".ovm/products/codex/versions")
        .join(version)
        .join("release");
    assert!(release_root.join("bin/codex").exists());
    assert!(release_root.join("meta.json").exists());
    assert!(release_root.join(".complete").exists());
    assert!(!release_root.join(".installing").exists());
}

/// Set up a mock server that serves two codex versions, sharing one asset payload.
fn setup_codex_mock_two_versions(
    version_a: &str,
    version_b: &str,
    binary_contents: &[u8],
) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_codex_asset();
    let asset_entry = expected_codex_entry();
    let asset_body = make_tarball(asset_entry, binary_contents);

    // Serve the same tarball bytes for every asset fetch in this test.
    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .expect_at_least(1)
        .create();

    for version in [version_a, version_b] {
        let asset_url = format!("{}/assets/{asset_name}", server.url());
        let release_json = format!(
            r#"{{"tag_name":"{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}"}}]}}"#,
        );
        server
            .mock("GET", format!("/tags/{version}").as_str())
            .with_status(200)
            .with_header("content-type", "application/json")
            .with_body(release_json)
            .create();
    }

    let base = server.url();
    (server, base)
}

#[test]
fn codex_uninstall_removes_inactive_version() {
    let home = tempfile::tempdir().expect("tempdir");
    let version_a = "rust-v0.120.0";
    let version_b = "rust-v0.119.0";
    let binary_contents = b"#!/bin/sh\necho fake-codex\n";

    let (_server, releases_url) =
        setup_codex_mock_two_versions(version_a, version_b, binary_contents);

    // Install both versions.
    ovm(home.path(), &releases_url)
        .args(["install", "codex", version_a])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["install", "codex", version_b])
        .assert()
        .success();

    let versions_dir: PathBuf = home.path().join(".ovm/products/codex/versions");
    assert!(versions_dir.join(version_a).exists());
    assert!(versions_dir.join(version_b).exists());

    // Activate version A so version B is inactive.
    ovm(home.path(), &releases_url)
        .args(["use", "codex", version_a])
        .assert()
        .success();

    // Uninstall the inactive version — should succeed and remove the dir.
    ovm(home.path(), &releases_url)
        .args(["uninstall", "codex", version_b])
        .assert()
        .success();

    assert!(
        !versions_dir.join(version_b).exists(),
        "inactive version directory should be removed by uninstall"
    );
    assert!(
        versions_dir.join(version_a).exists(),
        "active version should still be installed"
    );

    // `ls` should no longer list version B.
    use predicates::prelude::PredicateBooleanExt;
    ovm(home.path(), &releases_url)
        .args(["ls", "codex"])
        .assert()
        .success()
        .stdout(predicates::str::contains(version_a))
        .stdout(predicates::str::contains(version_b).not());
}

#[test]
fn explicit_use_records_pin_and_use_latest_clears_it() {
    let home = tempfile::tempdir().expect("tempdir");
    let newer = "rust-v0.120.0";
    let older = "rust-v0.119.0";
    let binary_contents = b"#!/bin/sh\necho fake-codex\n";

    let (_server, releases_url) = setup_codex_mock_two_versions(newer, older, binary_contents);

    for version in [newer, older] {
        ovm(home.path(), &releases_url)
            .args(["install", "codex", version])
            .assert()
            .success();
    }

    let pin: PathBuf = home.path().join(".ovm/products/codex/pinned");

    // Explicitly selecting a version records it as a deliberate pin.
    ovm(home.path(), &releases_url)
        .args(["use", "codex", older])
        .assert()
        .success();
    let pinned = std::fs::read_to_string(&pin).expect("pin file after explicit use");
    assert_eq!(pinned.trim(), older);

    // Opting back into latest-tracking removes the pin.
    ovm(home.path(), &releases_url)
        .args(["use", "codex", "latest"])
        .assert()
        .success();
    assert!(!pin.exists(), "`use latest` should clear the pin");
}
