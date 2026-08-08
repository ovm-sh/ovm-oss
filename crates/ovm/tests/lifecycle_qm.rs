//! End-to-end QM lifecycle against an isolated npm registry mock.

use assert_cmd::Command;
use base64::Engine as _;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Server, ServerGuard};
use predicates::prelude::PredicateBooleanExt as _;
use sha2::{Digest, Sha512};
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt as _;
use std::path::{Path, PathBuf};
use tar::Builder;

const OLD_VERSION: &str = "0.1.3";
const LATEST_VERSION: &str = "0.1.4";

fn make_qm_bundle(version: &str, include_package_json: bool) -> Vec<u8> {
    let mut bytes = Vec::new();
    {
        let encoder = GzEncoder::new(&mut bytes, Compression::default());
        let mut builder = Builder::new(encoder);
        let script =
            format!("#!/usr/bin/env -S node --\nprintf 'fake-qm-{version} args=%s\\n' \"$*\"\n");
        append(
            &mut builder,
            "package/dist/bin/qm.js",
            script.as_bytes(),
            0o755,
        );
        if include_package_json {
            let package = format!(
                r#"{{"name":"@yc-software/qm","version":"{version}","engines":{{"node":">=24.0.0"}}}}"#
            );
            append(
                &mut builder,
                "package/package.json",
                package.as_bytes(),
                0o644,
            );
        }
        let encoder = builder.into_inner().expect("finish tar");
        encoder.finish().expect("finish gzip");
    }
    bytes
}

fn append(builder: &mut Builder<GzEncoder<&mut Vec<u8>>>, name: &str, bytes: &[u8], mode: u32) {
    let mut header = tar::Header::new_gnu();
    header.set_size(bytes.len() as u64);
    header.set_mode(mode);
    header.set_cksum();
    builder
        .append_data(&mut header, name, bytes)
        .expect("append entry");
}

fn sri(bytes: &[u8]) -> String {
    format!(
        "sha512-{}",
        base64::engine::general_purpose::STANDARD.encode(Sha512::digest(bytes))
    )
}

fn setup_registry(include_latest_package_json: bool) -> (ServerGuard, String) {
    let mut server = Server::new();
    let old = make_qm_bundle(OLD_VERSION, true);
    let latest = make_qm_bundle(LATEST_VERSION, include_latest_package_json);
    let old_url = format!("{}/qm-{OLD_VERSION}.tgz", server.url());
    let latest_url = format!("{}/qm-{LATEST_VERSION}.tgz", server.url());

    server
        .mock("GET", format!("/qm-{OLD_VERSION}.tgz").as_str())
        .with_status(200)
        .with_body(old.clone())
        .create();
    server
        .mock("GET", format!("/qm-{LATEST_VERSION}.tgz").as_str())
        .with_status(200)
        .with_body(latest.clone())
        .create();

    let old_dist = serde_json::json!({"tarball": old_url, "integrity": sri(&old)});
    let latest_dist = serde_json::json!({"tarball": latest_url, "integrity": sri(&latest)});
    server
        .mock("GET", format!("/{OLD_VERSION}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::json!({"dist": old_dist.clone()}).to_string())
        .create();
    server
        .mock("GET", format!("/{LATEST_VERSION}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(serde_json::json!({"dist": latest_dist.clone()}).to_string())
        .create();

    let mut root = serde_json::json!({
        "versions": {},
        "dist-tags": {"latest": LATEST_VERSION}
    });
    root["versions"][OLD_VERSION] = serde_json::json!({"dist": old_dist});
    root["versions"][LATEST_VERSION] = serde_json::json!({"dist": latest_dist});
    server
        .mock("GET", "/")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(root.to_string())
        .create();

    let url = server.url();
    (server, url)
}

fn dead_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("dead port");
    let address = listener.local_addr().expect("address");
    drop(listener);
    format!("http://{address}")
}

fn supported_node(dir: &Path, version: &str) -> PathBuf {
    fs::create_dir_all(dir).expect("node dir");
    let path = dir.join("node");
    fs::write(
        &path,
        format!(
            "#!/bin/sh\nif [ \"${{1:-}}\" = --version ]; then echo v{version}; exit 0; fi\nif [ \"${{1:-}}\" = -- ]; then shift; fi\nexec /bin/sh \"$@\"\n"
        ),
    )
    .expect("node script");
    let mut permissions = fs::metadata(&path).expect("node metadata").permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&path, permissions).expect("node permissions");
    path
}

fn isolated_path(node_dir: &Path) -> String {
    format!(
        "{}:{}",
        node_dir.display(),
        std::env::var("PATH").unwrap_or_default()
    )
}

fn configure(command: &mut Command, home: &Path, registry: &str, path: &str) {
    let dead = dead_url();
    command
        .env("HOME", home)
        .env("PATH", path)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_QM_NPM_PACKAGE_URL", registry)
        .env("OVM_REGISTRY_BASE_URL", &dead)
        .env("OVM_NPM_PACKAGE_URL", &dead)
        .env("OVM_CODEX_RELEASES_URL", &dead)
        .env("OVM_CODEX_NPM_REGISTRY_URL", &dead)
        .env("OVM_PI_RELEASES_URL", &dead)
        .env("OVM_PI_NPM_REGISTRY_URL", &dead)
        .env("OVM_GITHUB_API_URL", &dead)
        .env_remove("OVM_PRODUCT")
        .env_remove("OVM_VERSION");
}

fn ovm(home: &Path, registry: &str, path: &str) -> Command {
    let mut command = Command::cargo_bin("ovm").expect("binary built");
    configure(&mut command, home, registry, path);
    command
}

fn launcher(home: &Path, registry: &str, path: &str) -> Command {
    let mut command = Command::new(home.join(".ovm/bin/qm"));
    configure(&mut command, home, registry, path);
    command
}

fn global_qm_install(root: &Path, version: &str) -> (PathBuf, PathBuf) {
    let package_entrypoint = root.join("lib/node_modules/@yc-software/qm/dist/bin/qm.js");
    fs::create_dir_all(package_entrypoint.parent().expect("package bin parent"))
        .expect("global package tree");
    fs::write(
        &package_entrypoint,
        format!("#!/bin/sh\nprintf '%s\\n' '{version}'\n"),
    )
    .expect("global qm entrypoint");
    let mut permissions = fs::metadata(&package_entrypoint)
        .expect("global qm metadata")
        .permissions();
    permissions.set_mode(0o755);
    fs::set_permissions(&package_entrypoint, permissions).expect("global qm executable");

    let global_bin = root.join("bin");
    fs::create_dir_all(&global_bin).expect("global bin");
    let launcher = global_bin.join("qm");
    std::os::unix::fs::symlink(&package_entrypoint, &launcher).expect("global qm launcher");
    (global_bin, launcher)
}

#[test]
fn qm_install_use_launch_list_and_uninstall_lifecycle() {
    let home = tempfile::tempdir().expect("home");
    let node_dir = tempfile::tempdir().expect("node dir");
    supported_node(node_dir.path(), "24.1.0");
    let path = isolated_path(node_dir.path());
    let (_server, registry) = setup_registry(true);

    ovm(home.path(), &registry, &path)
        .args(["ls", "qm", "--remote"])
        .assert()
        .success()
        .stdout(predicates::str::contains(OLD_VERSION))
        .stdout(predicates::str::contains(LATEST_VERSION));

    ovm(home.path(), &registry, &path)
        .args(["install", "qm", "latest"])
        .assert()
        .success();

    let release = home
        .path()
        .join(".ovm/products/qm/versions")
        .join(LATEST_VERSION)
        .join("release");
    assert!(release.join(".complete").is_file());
    assert!(release.join("meta.json").is_file());
    assert!(release.join("bundle/package/package.json").is_file());
    assert!(release.join("bundle/package/dist").is_dir());
    assert!(release.join("bundle/package/dist/bin/qm.js").is_file());

    ovm(home.path(), &registry, &path)
        .args(["use", "qm", LATEST_VERSION])
        .assert()
        .success();
    let generated_launcher = home.path().join(".ovm/bin/qm");
    assert!(
        generated_launcher.is_symlink(),
        "the code under test must create the QM launcher"
    );

    ovm(home.path(), &registry, &path)
        .args(["qm", "--version"])
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION} args=--version"
        )));
    launcher(home.path(), &registry, &path)
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION} args="
        )));
    launcher(home.path(), &registry, &path)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION} args=--version"
        )));
    launcher(home.path(), &registry, &path)
        .arg("update")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION} args=update"
        )));

    ovm(home.path(), &registry, &path)
        .args(["install", "qm", OLD_VERSION])
        .assert()
        .success();
    ovm(home.path(), &registry, &path)
        .args(["ls", "qm"])
        .assert()
        .success()
        .stdout(predicates::str::contains(OLD_VERSION))
        .stdout(predicates::str::contains(LATEST_VERSION));
    ovm(home.path(), &registry, &path)
        .args(["uninstall", "qm", OLD_VERSION])
        .assert()
        .success();
    assert!(!home
        .path()
        .join(".ovm/products/qm/versions")
        .join(OLD_VERSION)
        .exists());
}

#[test]
fn qm_node_preflight_covers_install_and_exec() {
    let home = tempfile::tempdir().expect("home");
    let empty_path = tempfile::tempdir().expect("empty path");
    let old_node = tempfile::tempdir().expect("old node");
    let new_node = tempfile::tempdir().expect("new node");
    supported_node(old_node.path(), "23.11.1");
    supported_node(new_node.path(), "24.0.0");
    let (_server, registry) = setup_registry(true);

    ovm(
        home.path(),
        &registry,
        empty_path.path().to_str().expect("path"),
    )
    .args(["install", "qm", LATEST_VERSION])
    .assert()
    .failure()
    .stderr(predicates::str::contains("not found on PATH"));
    assert!(!home
        .path()
        .join(format!(
            ".ovm/products/qm/versions/{LATEST_VERSION}/release/.complete"
        ))
        .exists());

    ovm(
        home.path(),
        &registry,
        old_node.path().to_str().expect("path"),
    )
    .args(["install", "qm", LATEST_VERSION])
    .assert()
    .failure()
    .stderr(predicates::str::contains("Node.js 23.11.1"))
    .stderr(predicates::str::contains("Node.js 24 or newer"));

    let supported_path = isolated_path(new_node.path());
    ovm(home.path(), &registry, &supported_path)
        .args(["install", "qm", LATEST_VERSION])
        .assert()
        .success();
    ovm(home.path(), &registry, &supported_path)
        .args(["use", "qm", LATEST_VERSION])
        .assert()
        .success();

    launcher(
        home.path(),
        &registry,
        empty_path.path().to_str().expect("path"),
    )
    .arg("--version")
    .assert()
    .failure()
    .stderr(predicates::str::contains("not found on PATH"));
    launcher(
        home.path(),
        &registry,
        old_node.path().to_str().expect("path"),
    )
    .arg("--version")
    .assert()
    .failure()
    .stderr(predicates::str::contains("Node.js 23.11.1"));
    launcher(home.path(), &registry, &supported_path)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION}"
        )));
}

#[test]
fn qm_missing_required_bundle_member_never_publishes_complete() {
    let home = tempfile::tempdir().expect("home");
    let node_dir = tempfile::tempdir().expect("node dir");
    supported_node(node_dir.path(), "24.0.0");
    let path = isolated_path(node_dir.path());
    let (_server, registry) = setup_registry(false);

    ovm(home.path(), &registry, &path)
        .args(["install", "qm", LATEST_VERSION])
        .assert()
        .failure()
        .stderr(predicates::str::contains("package/package.json"));
    assert!(!home
        .path()
        .join(format!(
            ".ovm/products/qm/versions/{LATEST_VERSION}/release/.complete"
        ))
        .exists());
}

#[test]
fn qm_adopt_migrates_a_shadowing_global_npm_install_to_the_managed_bundle() {
    let home = tempfile::tempdir().expect("home");
    let global = tempfile::tempdir().expect("global npm prefix");
    let node_dir = tempfile::tempdir().expect("node dir");
    supported_node(node_dir.path(), "24.1.0");
    let (global_bin, global_launcher) = global_qm_install(global.path(), LATEST_VERSION);
    let path = format!(
        "{}:{}:{}",
        global_bin.display(),
        node_dir.path().display(),
        std::env::var("PATH").unwrap_or_default()
    );
    let (_server, registry) = setup_registry(true);

    ovm(home.path(), &registry, &path)
        .args(["adopt", "qm"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "downloading managed QM 0.1.4 instead",
        ))
        .stdout(predicates::str::contains(
            "npm uninstall -g @yc-software/qm",
        ))
        .stderr(predicates::str::contains(
            "PATH has not taken over for `qm`",
        ))
        .stderr(predicates::str::contains(
            global_launcher.display().to_string(),
        ))
        .stderr(predicates::str::contains(
            "Keep the original install until `qm` resolves to OVM",
        ));

    assert!(
        global_launcher.is_symlink(),
        "the original global shim survives"
    );
    assert!(home
        .path()
        .join(".ovm/products/qm/versions/0.1.4/release/.complete")
        .is_file());
    ovm(home.path(), &registry, &path)
        .args(["current", "qm"])
        .assert()
        .success()
        .stdout(predicates::str::contains(LATEST_VERSION));

    let migrated_path = format!(
        "{}:{}:{}",
        home.path().join(".ovm/bin").display(),
        global_bin.display(),
        node_dir.path().display()
    );
    launcher(home.path(), &registry, &migrated_path)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains(format!(
            "fake-qm-{LATEST_VERSION}"
        )));
}

#[test]
fn qm_doctor_never_reports_a_clean_check_that_did_not_run() {
    let home = tempfile::tempdir().expect("home");
    let node_dir = tempfile::tempdir().expect("node dir");
    supported_node(node_dir.path(), "24.1.0");
    let path = isolated_path(node_dir.path());
    let (_server, registry) = setup_registry(true);

    ovm(home.path(), &registry, &path)
        .args(["doctor", "qm"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Doctor checks are not implemented for QM.",
        ))
        .stdout(predicates::str::contains("nothing to check").not());
}
