//! Lifecycle test for Pi — uses the bundle-extract path
//! (Pi ships as a directory bundle with package.json + binary).

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, Server, ServerGuard};
use predicates::prelude::PredicateBooleanExt;
use std::fs;
use std::path::Path;
use tar::Builder;

/// Build a gzipped tar archive containing a `pi/` directory with a `pi` binary
/// and a `package.json`. This mirrors the real Pi release layout.
fn make_pi_bundle(binary_contents: &[u8]) -> Vec<u8> {
    let mut buf = Vec::new();
    {
        let encoder = GzEncoder::new(&mut buf, Compression::default());
        let mut builder = Builder::new(encoder);

        // pi/pi (the binary)
        let mut hdr = tar::Header::new_gnu();
        hdr.set_size(binary_contents.len() as u64);
        hdr.set_mode(0o755);
        hdr.set_cksum();
        builder
            .append_data(&mut hdr, "pi/pi", binary_contents)
            .expect("append pi");

        // pi/package.json (required by the Bun runtime at startup)
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

fn ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_PI_RELEASES_URL", releases_url)
        .env("OVM_PI_NPM_REGISTRY_URL", releases_url);
    cmd
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

fn setup_pi_mock(version: &str, binary_contents: &[u8]) -> (ServerGuard, String) {
    let mut server = Server::new();

    let asset_name = expected_pi_asset();
    let asset_body = make_pi_bundle(binary_contents);
    let asset_size = asset_body.len();

    // Asset download endpoint
    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(asset_body)
        .create();

    // /tags/v<version> — Pi normalizes to "v" prefix
    let asset_url = format!("{}/assets/{asset_name}", server.url());
    let release_json = format!(
        r#"{{"tag_name":"v{version}","assets":[{{"name":"{asset_name}","browser_download_url":"{asset_url}","size":{asset_size}}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/v{version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json.clone())
        .create();
    server
        .mock("GET", "/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(release_json)
        .create();

    // Version list (for ls --remote)
    server
        .mock("GET", "/")
        .match_query(Matcher::UrlEncoded("page".into(), "1".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!(r#"[{{"tag_name":"v{version}","assets":[]}}]"#))
        .create();
    server
        .mock("GET", "/")
        .match_query(Matcher::UrlEncoded("page".into(), "2".into()))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body("[]")
        .create();

    let base = server.url();
    (server, base)
}

fn setup_pi_update_mock(
    old_version: &str,
    old_binary: &[u8],
    latest_version: &str,
    latest_binary: &[u8],
) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_pi_asset();

    let old_asset_body = make_pi_bundle(old_binary);
    let old_asset_size = old_asset_body.len();
    server
        .mock(
            "GET",
            format!("/assets/{old_version}/{asset_name}").as_str(),
        )
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(old_asset_body)
        .create();

    let latest_asset_body = make_pi_bundle(latest_binary);
    let latest_asset_size = latest_asset_body.len();
    server
        .mock(
            "GET",
            format!("/assets/{latest_version}/{asset_name}").as_str(),
        )
        .with_status(200)
        .with_header("content-type", "application/octet-stream")
        .with_body(latest_asset_body)
        .create();

    let old_asset_url = format!("{}/assets/{old_version}/{asset_name}", server.url());
    let old_release_json = format!(
        r#"{{"tag_name":"v{old_version}","assets":[{{"name":"{asset_name}","browser_download_url":"{old_asset_url}","size":{old_asset_size}}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/v{old_version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(old_release_json)
        .create();

    let latest_asset_url = format!("{}/assets/{latest_version}/{asset_name}", server.url());
    let latest_release_json = format!(
        r#"{{"tag_name":"v{latest_version}","assets":[{{"name":"{asset_name}","browser_download_url":"{latest_asset_url}","size":{latest_asset_size}}}]}}"#,
    );
    server
        .mock("GET", format!("/tags/v{latest_version}").as_str())
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(latest_release_json.clone())
        .create();
    server
        .mock("GET", "/latest")
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(latest_release_json)
        .create();

    let base = server.url();
    (server, base)
}

#[test]
fn pi_full_install_and_activate() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.67.6";
    let binary_contents = b"#!/bin/sh\necho fake-pi\n";

    let (_server, releases_url) = setup_pi_mock(version, binary_contents);

    // Install
    ovm(home.path(), &releases_url)
        .args(["install", "pi", version])
        .assert()
        .success();

    // Bundle extracted correctly — binary AND package.json both present
    let bundle_dir = home
        .path()
        .join(".ovm/products/pi/versions")
        .join(version)
        .join("release/bundle");
    let binary = bundle_dir.join("pi/pi");
    let pkg = bundle_dir.join("pi/package.json");
    assert!(binary.exists(), "pi binary should be in the bundle");
    assert!(pkg.exists(), "package.json must be extracted alongside");
    assert_eq!(fs::read(&binary).expect("read binary"), binary_contents);

    // Activate
    ovm(home.path(), &releases_url)
        .args(["use", "pi", version])
        .assert()
        .success();

    // Verify symlink path resolves to the bundled binary
    ovm(home.path(), &releases_url)
        .args(["which", "pi"])
        .assert()
        .success()
        .stdout(predicates::str::contains("release/bundle/pi/pi"));

    let launcher = home.path().join(".ovm/bin/pi");
    assert!(
        launcher.is_symlink(),
        "pi launcher should be OVM-owned after activation"
    );

    // Stats shows 1 installed
    ovm(home.path(), &releases_url)
        .arg("stats")
        .assert()
        .success()
        .stdout(predicates::str::contains("Pi"))
        .stdout(predicates::str::contains("installed: 1"));
}

/// `clean` removes cached download artifacts, and its message must say that —
/// "Cleaned all pi versions, freed 0 B" claimed a version removal that never
/// happens (the absence-rendered-as-a-verdict shape the devlogs catalogue).
#[test]
fn clean_reports_cached_artifacts_not_version_removal() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.67.6";
    let (_server, releases_url) = setup_pi_mock(version, b"#!/bin/sh\necho fake-pi\n");

    ovm(home.path(), &releases_url)
        .args(["install", "pi", version])
        .assert()
        .success();

    ovm(home.path(), &releases_url)
        .args(["clean", "pi", "--all"])
        .assert()
        .success()
        .stdout(
            predicates::str::contains("cached download artifacts")
                .and(predicates::str::contains("Cleaned all").not()),
        );

    // The install itself must survive a clean.
    let bundle = home
        .path()
        .join(".ovm/products/pi/versions")
        .join(version)
        .join("release/bundle/pi/pi");
    assert!(
        bundle.exists(),
        "clean must not touch the installed version"
    );
}

/// `uninstall --all` is the way to fully leave a product: active version
/// included, selection cleared. Non-interactive shells cannot answer the
/// typed confirmation, so without --yes they get a refusal that says how to
/// proceed, and a piped "yes" is not a person confirming.
#[test]
fn uninstall_all_leaves_the_product_entirely() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.67.6";
    let (_server, releases_url) = setup_pi_mock(version, b"#!/bin/sh\necho fake-pi\n");

    ovm(home.path(), &releases_url)
        .args(["install", "pi", version])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["use", "pi", version])
        .assert()
        .success();

    // The active version alone still refuses a plain uninstall.
    ovm(home.path(), &releases_url)
        .args(["uninstall", "pi", version])
        .assert()
        .failure();

    // Non-interactive without --yes: refuse, with the way forward spelled out.
    ovm(home.path(), &releases_url)
        .args(["uninstall", "pi", "--all"])
        .write_stdin("yes\n")
        .assert()
        .failure()
        .stderr(predicates::str::contains("--yes"));

    ovm(home.path(), &releases_url)
        .args(["uninstall", "pi", "--all", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("Uninstalled all 1 Pi version"));

    let versions_dir = home.path().join(".ovm/products/pi/versions");
    let remaining = fs::read_dir(&versions_dir)
        .map(|entries| entries.count())
        .unwrap_or(0);
    assert_eq!(remaining, 0, "every version directory must be gone");
    assert!(
        !home.path().join(".ovm/products/pi/current").is_symlink(),
        "the active selection must be cleared with the versions"
    );

    // A second pass finds nothing and says so without erroring.
    ovm(home.path(), &releases_url)
        .args(["uninstall", "pi", "--all", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("No Pi versions are installed"));

    // Leftovers alone — a partial tree from an interrupted download — must
    // still be sweepable, not early-exited past as "nothing installed".
    fs::create_dir_all(versions_dir.join("0.99.9")).expect("partial leftover");
    ovm(home.path(), &releases_url)
        .args(["uninstall", "pi", "--all", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains(
            "Removed 1 leftover partial install directory",
        ));
    assert!(!versions_dir.exists(), "the leftover sweep must run");
}

/// `install` must mention an unmanaged copy on PATH — it may shadow the
/// managed install, and someone who never runs `launch` or `adopt` used to
/// never hear about it.
#[test]
fn install_points_at_an_unmanaged_copy_on_path() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.67.6";
    let (_server, releases_url) = setup_pi_mock(version, b"#!/bin/sh\necho fake-pi\n");

    let foreign_dir = home.path().join("foreign-bin");
    fs::create_dir_all(&foreign_dir).expect("mkdir");
    let foreign = foreign_dir.join("pi");
    fs::write(&foreign, "#!/bin/sh\necho foreign-pi\n").expect("write");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&foreign, fs::Permissions::from_mode(0o755)).expect("chmod");
    }

    ovm(home.path(), &releases_url)
        .env("PATH", format!("{}:/usr/bin:/bin", foreign_dir.display()))
        .args(["install", "pi", version])
        .assert()
        .success()
        .stderr(
            predicates::str::contains("unmanaged").and(predicates::str::contains("ovm adopt pi")),
        );
}

#[test]
fn pi_latest_install_uses_normalized_version_directory() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.74.0";
    let binary_contents = b"#!/bin/sh\necho fake-pi-latest\n";

    let (_server, releases_url) = setup_pi_mock(version, binary_contents);

    ovm(home.path(), &releases_url)
        .args(["install", "pi", "latest"])
        .assert()
        .success();

    let normalized_dir = home.path().join(".ovm/products/pi/versions").join(version);
    let prefixed_dir = home
        .path()
        .join(".ovm/products/pi/versions")
        .join(format!("v{version}"));

    assert!(
        normalized_dir.exists(),
        "normalized version dir should exist"
    );
    assert!(
        !prefixed_dir.exists(),
        "v-prefixed version dir should not be created for Pi"
    );

    ovm(home.path(), &releases_url)
        .args(["use", "pi", "latest"])
        .assert()
        .success()
        .stderr(predicates::str::contains(format!("Now using Pi {version}")));

    ovm(home.path(), &releases_url)
        .args(["current", "pi"])
        .assert()
        .success()
        .stdout(predicates::str::contains(version));

    ovm(home.path(), &releases_url)
        .args(["pi", "latest", "--version"])
        .assert()
        .success()
        .stderr(predicates::str::contains("latest not found").not())
        .stdout(predicates::str::contains("fake-pi-latest"));
}

/// Regression: when ovm is invoked as the `pi` owned launcher, the hidden
/// `__refresh-cache` sentinel must run the background refresh and exit — it must
/// NOT be routed into a Pi launch. Routing it into a launch re-armed the refresh
/// spawner without clearing the "due" flag, which fork-bombed the machine
/// (thousands of `pi __refresh-cache` processes).
#[test]
fn pi_launcher_refresh_cache_sentinel_does_not_relaunch_pi() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "0.79.8";
    // A pi binary that announces every invocation, so a launch would be visible.
    let pi_binary = b"#!/bin/sh\necho \"pi-launched args=$*\"\n";

    let (_server, releases_url) = setup_pi_mock(version, pi_binary);

    ovm(home.path(), &releases_url)
        .args(["install", "pi", version])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["use", "pi", version])
        .assert()
        .success();

    // Invoke through the OVM-owned `pi` launcher (argv[0] == "pi"), exactly as
    // the background spawner does. Point the registry at the dead mock base so
    // the refresh stays hermetic and fast instead of reaching the real network.
    let launcher = home.path().join(".ovm/bin/pi");
    Command::new(&launcher)
        .env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_PI_RELEASES_URL", &releases_url)
        .env("OVM_PI_NPM_REGISTRY_URL", &releases_url)
        .env("OVM_REGISTRY_BASE_URL", &releases_url)
        .arg("__refresh-cache")
        .assert()
        .success()
        .stdout(predicates::str::contains("pi-launched").not());
}

#[test]
fn bare_pi_update_updates_extensions_then_switches_latest_via_ovm() {
    let home = tempfile::tempdir().expect("tempdir");
    let old_version = "0.74.0";
    let latest_version = "0.79.8";
    let old_binary = b"#!/bin/sh\necho \"old-pi args=$*\"\n";
    let latest_binary = b"#!/bin/sh\necho \"latest-pi args=$*\"\n";

    let (_server, releases_url) =
        setup_pi_update_mock(old_version, old_binary, latest_version, latest_binary);

    ovm(home.path(), &releases_url)
        .args(["install", "pi", old_version])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["use", "pi", old_version])
        .assert()
        .success();

    let launcher = home.path().join(".ovm/bin/pi");
    Command::new(&launcher)
        .env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_PI_RELEASES_URL", &releases_url)
        .env("OVM_PI_NPM_REGISTRY_URL", &releases_url)
        .arg("update")
        .assert()
        .success()
        .stdout(predicates::str::contains("old-pi args=update --extensions"))
        .stderr(predicates::str::contains(format!(
            "Updated Pi {old_version} -> {latest_version}"
        )));

    ovm(home.path(), &releases_url)
        .args(["current", "pi"])
        .assert()
        .success()
        .stdout(predicates::str::contains(latest_version));

    Command::new(&launcher)
        .env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_PI_RELEASES_URL", &releases_url)
        .env("OVM_PI_NPM_REGISTRY_URL", &releases_url)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::contains("latest-pi args=--version"));
}
