//! `ovm version` — the "what have I got?" screen.
//!
//! Its whole value is being honest about state it did not have to fetch: an
//! empty store must say "not installed" (with the line that fixes it), and an
//! installed product must report the active version rather than the newest on
//! disk. It must also never reach the network — this is the command people run
//! on a plane, and a hang here reads as a broken install.

use assert_cmd::Command;
use flate2::write::GzEncoder;
use flate2::Compression;
use mockito::{Matcher, Server, ServerGuard};
use std::path::Path;
use std::time::Duration;

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

fn setup_codex_mock(version: &str) -> (ServerGuard, String) {
    let mut server = Server::new();
    let asset_name = expected_codex_asset();
    let body = make_tarball(expected_codex_entry(), b"#!/bin/sh\necho fake-codex\n");
    let size = body.len();

    server
        .mock("GET", format!("/assets/{asset_name}").as_str())
        .with_status(200)
        .with_body(body)
        .expect_at_least(0)
        .create();

    let asset_url = format!("{}/assets/{asset_name}", server.url());
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
    server
        .mock("GET", "/")
        .match_query(Matcher::AllOf(vec![
            Matcher::UrlEncoded("per_page".into(), "100".into()),
            Matcher::UrlEncoded("page".into(), "1".into()),
        ]))
        .with_status(200)
        .with_header("content-type", "application/json")
        .with_body(format!("[{json}]"))
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

fn ovm(home: &Path, releases_url: &str) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_CODEX_RELEASES_URL", releases_url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

#[test]
fn an_empty_store_says_so_for_every_product_and_names_the_fix() {
    let home = tempfile::tempdir().expect("tempdir");
    let output = ovm(home.path(), "http://127.0.0.1:9")
        .arg("version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(text.contains("ovm "), "reports its own version: {text}");
    assert!(text.contains(env!("CARGO_PKG_VERSION")), "{text}");
    for product in ["claude", "codex", "pi"] {
        assert!(text.contains(product), "lists {product}: {text}");
    }
    assert!(text.contains("not installed"), "{text}");
    assert!(
        text.contains("ovm install claude latest"),
        "offers the next step: {text}"
    );
}

#[test]
fn an_installed_product_reports_the_active_version_and_the_count() {
    let home = tempfile::tempdir().expect("tempdir");
    let version = "rust-v0.120.0";
    let (_server, releases_url) = setup_codex_mock(version);

    ovm(home.path(), &releases_url)
        .args(["install", "codex", version])
        .assert()
        .success();
    ovm(home.path(), &releases_url)
        .args(["use", "codex", version])
        .assert()
        .success();

    let output = ovm(home.path(), &releases_url)
        .arg("version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(text.contains(version), "shows the active version: {text}");
    assert!(text.contains("1 installed"), "counts the store: {text}");
    // `ovm use` pins, and a pin is the thing that would stop an update.
    assert!(text.contains("pinned"), "surfaces the pin: {text}");
}

#[test]
fn it_answers_offline_because_it_reads_only_local_state() {
    // Every product source points at a closed port. A version report that
    // needed the network would hang or fail here.
    let home = tempfile::tempdir().expect("tempdir");
    let dead = "http://127.0.0.1:9";

    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_CODEX_RELEASES_URL", dead)
        .env("OVM_CODEX_NPM_REGISTRY_URL", dead)
        .env("OVM_NPM_PACKAGE_URL", dead)
        .env("OVM_PI_RELEASES_URL", dead)
        .env("OVM_PI_NPM_REGISTRY_URL", dead)
        .env("OVM_REGISTRY_BASE_URL", dead)
        .env("OVM_GITHUB_API_URL", dead)
        .env_remove("OVM_VERSION")
        .arg("version")
        .timeout(Duration::from_secs(10))
        .assert()
        .success();
}

#[test]
fn the_flag_still_prints_just_the_version_for_scripts() {
    // `ovm --version` is what a script parses; the subcommand is for people.
    let home = tempfile::tempdir().expect("tempdir");
    let output = Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", home.path())
        .arg("--version")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let text = String::from_utf8_lossy(&output);

    assert!(text.contains(env!("CARGO_PKG_VERSION")));
    assert!(
        !text.contains("not installed"),
        "the flag stays terse: {text}"
    );
}
