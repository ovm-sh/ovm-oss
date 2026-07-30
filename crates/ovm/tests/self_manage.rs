#![cfg(unix)]

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::Server;
use predicates::prelude::*;
use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use tar::Builder;
use tempfile::tempdir;

const MAIN_ONLY_MANIFEST: &str = "ovm-bundle-v1\nmain\tovm\tovm\n";
const SIDE_MANIFEST: &str = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-side\t-\n";

fn write_executable(path: &Path, contents: &[u8]) {
    fs::write(path, contents).unwrap();
    let mut permissions = fs::metadata(path).unwrap().permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(path, permissions).unwrap();
}

fn write_version(root: &Path, version: &str, manifest: &str, main: &[u8]) {
    let version_dir = root.join(".ovm/self/versions").join(version);
    fs::create_dir_all(&version_dir).unwrap();
    fs::write(version_dir.join("ovm-bundle-v1.tsv"), manifest).unwrap();
    write_executable(&version_dir.join("ovm"), main);
    if manifest.contains("ovm-side") {
        write_executable(
            &version_dir.join("ovm-side"),
            b"#!/bin/sh\nprintf 'old-side:%s\\n' \"$*\"\n",
        );
    }
    fs::write(version_dir.join(".complete"), b"").unwrap();
}

fn write_release_archive(path: &Path, ovm: &[u8]) {
    let encoder = GzEncoder::new(File::create(path).unwrap(), Compression::default());
    let mut builder = Builder::new(encoder);
    for (name, contents, mode) in [
        ("ovm-bundle-v1.tsv", MAIN_ONLY_MANIFEST.as_bytes(), 0o644),
        ("ovm", ovm, 0o755),
    ] {
        let mut header = tar::Header::new_gnu();
        header.set_size(contents.len() as u64);
        header.set_mode(mode);
        header.set_cksum();
        builder.append_data(&mut header, name, contents).unwrap();
    }
    builder.finish().unwrap();
}

fn target_triple() -> &'static str {
    match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "aarch64-apple-darwin",
        ("macos", "x86_64") => "x86_64-apple-darwin",
        ("linux", "aarch64") => "aarch64-unknown-linux-gnu",
        ("linux", "x86_64") => "x86_64-unknown-linux-gnu",
        pair => panic!("unsupported test target: {pair:?}"),
    }
}

fn hex_digest(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[test]
fn control_plane_switches_old_versions_and_can_always_rollback() {
    let temp = tempdir().unwrap();
    let home = temp.path();
    let bin = home.join(".ovm/bin");
    let self_root = home.join(".ovm/self");
    fs::create_dir_all(&bin).unwrap();
    fs::create_dir_all(&self_root).unwrap();

    let built_ovm = assert_cmd::cargo::cargo_bin!("ovm");
    fs::copy(built_ovm, bin.join("ovm")).unwrap();
    fs::write(
        self_root.join("launcher-dir"),
        format!("{}\n", bin.display()),
    )
    .unwrap();

    write_version(
        home,
        "old",
        SIDE_MANIFEST,
        b"#!/bin/sh\nprintf 'old-main:%s\\n' \"$*\"\n",
    );
    write_version(
        home,
        "new",
        MAIN_ONLY_MANIFEST,
        &fs::read(built_ovm).unwrap(),
    );
    symlink(self_root.join("versions/old"), self_root.join("current")).unwrap();
    symlink("ovm", bin.join("ovm-side")).unwrap();
    symlink("ovm", bin.join("codex")).unwrap();

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .arg("--version")
        .assert()
        .success()
        .stdout("old-main:--version\n");

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "current"])
        .assert()
        .success()
        .stdout("old\n");

    std::process::Command::new(bin.join("ovm-side"))
        .env("HOME", home)
        .arg("probe")
        .output()
        .map(|output| {
            assert!(output.status.success());
            assert_eq!(String::from_utf8_lossy(&output.stdout), "old-side:probe\n");
        })
        .unwrap();

    fs::remove_file(bin.join("codex")).unwrap();
    symlink(self_root.join("versions/old/ovm"), bin.join("codex")).unwrap();
    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "use", "new"])
        .assert()
        .success();
    assert_eq!(fs::read_link(bin.join("codex")).unwrap(), bin.join("ovm"));
    assert!(!bin.join("ovm-side").exists());

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::contains(env!("CARGO_PKG_VERSION")));

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "rollback"])
        .assert()
        .success();
    assert_eq!(
        fs::read_link(bin.join("ovm-side")).unwrap(),
        Path::new("ovm")
    );

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .arg("--version")
        .assert()
        .success()
        .stdout("old-main:--version\n");
}

#[test]
fn direct_update_dry_run_reads_metadata_without_creating_state() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let mut server = Server::new();
    let archive_name = format!("ovm-{}.tar.gz", target_triple());
    let metadata = serde_json::json!({
        "tag_name": "v9.9.9",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": archive_name,
                "browser_download_url": format!("{}/archive", server.url())
            },
            {
                "name": format!("{archive_name}.sha256"),
                "browser_download_url": format!("{}/checksum", server.url())
            }
        ]
    });
    let metadata_mock = server
        .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(metadata.to_string())
        .create();

    Command::cargo_bin("ovm")
        .unwrap()
        .env("HOME", &home)
        .env("OVM_GITHUB_API_URL", server.url())
        .args(["self", "update", "--method", "direct", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("release 9.9.9"));

    metadata_mock.assert();
    assert!(!home.join(".ovm").exists());
}

#[test]
fn direct_update_dry_run_rejects_a_release_without_platform_assets() {
    let temp = tempdir().unwrap();
    let home = temp.path().join("home");
    fs::create_dir_all(&home).unwrap();

    let mut server = Server::new();
    let metadata = serde_json::json!({
        "tag_name": "v9.9.9",
        "draft": false,
        "prerelease": false,
        "assets": []
    });
    let metadata_mock = server
        .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(metadata.to_string())
        .create();

    Command::cargo_bin("ovm")
        .unwrap()
        .env("HOME", &home)
        .env("OVM_GITHUB_API_URL", server.url())
        .args(["self", "update", "--method", "direct", "--dry-run"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("missing asset"));

    metadata_mock.assert();
    assert!(!home.join(".ovm").exists());
}

#[test]
fn direct_update_downloads_verifies_and_activates_release() {
    let temp = tempdir().unwrap();
    let home = temp.path();
    let bin = home.join(".ovm/bin");
    let self_root = home.join(".ovm/self");
    fs::create_dir_all(&bin).unwrap();

    let built_ovm = assert_cmd::cargo::cargo_bin!("ovm");
    fs::copy(built_ovm, bin.join("ovm")).unwrap();
    let archive_path = temp.path().join("release.tar.gz");
    write_release_archive(&archive_path, &fs::read(built_ovm).unwrap());
    let archive = fs::read(&archive_path).unwrap();
    let digest = hex_digest(&archive);
    let archive_name = format!("ovm-{}.tar.gz", target_triple());

    let mut server = Server::new();
    let metadata = serde_json::json!({
        "tag_name": "v9.9.9",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": archive_name,
                "browser_download_url": format!("{}/archive", server.url())
            },
            {
                "name": format!("{archive_name}.sha256"),
                "browser_download_url": format!("{}/checksum", server.url())
            }
        ]
    });
    let metadata_mock = server
        .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(metadata.to_string())
        .create();
    let archive_mock = server
        .mock("GET", "/archive")
        .with_status(200)
        .with_body(archive)
        .create();
    let checksum_mock = server
        .mock("GET", "/checksum")
        .with_status(200)
        .with_body(format!("{digest}  {archive_name}\n"))
        .create();

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .env("OVM_GITHUB_API_URL", server.url())
        .args(["self", "update", "--method", "direct"])
        .assert()
        .success();

    let current = fs::read_link(home.join(".ovm/self/current")).unwrap();
    assert!(current.ends_with("versions/9.9.9"));
    assert!(current.join(".complete").is_file());
    assert!(bin.join("ovm").is_file());
    assert!(!bin.join("ovm").is_symlink());

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "current"])
        .assert()
        .success()
        .stdout("9.9.9\n");

    metadata_mock.assert();
    archive_mock.assert();
    checksum_mock.assert();

    write_version(
        home,
        "9.9.8",
        MAIN_ONLY_MANIFEST,
        &fs::read(built_ovm).unwrap(),
    );
    symlink(self_root.join("versions/9.9.8"), self_root.join("previous")).unwrap();
    write_executable(
        &self_root.join("control-previous"),
        b"original-control-backup",
    );

    let bad_archive_path = temp.path().join("bad-release.tar.gz");
    write_release_archive(&bad_archive_path, b"#!/bin/sh\nexit 42\n");
    let bad_archive = fs::read(&bad_archive_path).unwrap();
    let bad_digest = hex_digest(&bad_archive);
    let mut bad_server = Server::new();
    let bad_metadata = serde_json::json!({
        "tag_name": "v10.0.0",
        "draft": false,
        "prerelease": false,
        "assets": [
            {
                "name": archive_name,
                "browser_download_url": format!("{}/archive", bad_server.url())
            },
            {
                "name": format!("{archive_name}.sha256"),
                "browser_download_url": format!("{}/checksum", bad_server.url())
            }
        ]
    });
    let bad_metadata_mock = bad_server
        .mock("GET", "/repos/ovm-sh/ovm-oss/releases/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(bad_metadata.to_string())
        .create();
    let bad_archive_mock = bad_server
        .mock("GET", "/archive")
        .with_status(200)
        .with_body(bad_archive)
        .create();
    let bad_checksum_mock = bad_server
        .mock("GET", "/checksum")
        .with_status(200)
        .with_body(format!("{bad_digest}  {archive_name}\n"))
        .create();

    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .env("OVM_GITHUB_API_URL", bad_server.url())
        .args(["self", "update", "--method", "direct"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("failed its activation probe"));
    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "current"])
        .assert()
        .success()
        .stdout("9.9.9\n");
    assert!(fs::read_link(self_root.join("previous"))
        .unwrap()
        .ends_with("versions/9.9.8"));
    assert_eq!(
        fs::read(self_root.join("control-previous")).unwrap(),
        b"original-control-backup"
    );
    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "rollback"])
        .assert()
        .success();
    Command::new(bin.join("ovm"))
        .env("HOME", home)
        .args(["self", "current"])
        .assert()
        .success()
        .stdout("9.9.8\n");

    bad_metadata_mock.assert();
    bad_archive_mock.assert();
    bad_checksum_mock.assert();
}
