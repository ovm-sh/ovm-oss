//! `ovm tour` — the interactive contract.
//!
//! The tour is a terminal conversation, so almost all of it can only be
//! exercised by a person. What a test CAN pin down is the boundary: run
//! without a terminal it must refuse with the scripted alternative rather
//! than hang waiting for a keypress that will never come — `curl | sh`
//! pipelines and CI both hit exactly this path.

use assert_cmd::Command;

#[test]
fn tour_without_a_terminal_refuses_and_names_the_scripted_path() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    let assert = cmd
        .env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .arg("tour")
        .write_stdin("2\n")
        .assert()
        .failure();
    let stderr = String::from_utf8_lossy(&assert.get_output().stderr).to_string();
    assert!(
        stderr.contains("interactive"),
        "should explain why it refused: {stderr}"
    );
    assert!(
        stderr.contains("ovm install"),
        "should name the scripted alternative: {stderr}"
    );
}

#[test]
fn tour_is_listed_in_help() {
    let home = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    let assert = cmd
        .env("HOME", home.path())
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .arg("--help")
        .assert()
        .success();
    let stdout = String::from_utf8_lossy(&assert.get_output().stdout).to_string();
    assert!(stdout.contains("tour"), "help should list tour: {stdout}");
}
