//! The auto-update child's parent watchdog, from the outside.
//!
//! `OVM_EXIT_WITH_PARENT` is how the skippable launch auto-update tells its
//! child install "do not outlive me". The dangerous half — a watching child
//! exits when its launcher dies — is process-lifecycle code exercised in
//! `autoupdate`'s unit tests; what an integration test must pin is the SAFE
//! half: the variable arms nothing it should not. The first process to read
//! it removes it, so hooks and npm children normally never see it — but a
//! value can still arrive unearned (leaked from a shell, an exported debug
//! session), and an over-eager match would make that invocation kill itself
//! at startup. Arming is therefore gated on the value naming the CURRENT
//! parent, with a definitive ESRCH on the named PID required before the
//! dead-launcher early exit.

use assert_cmd::Command;

/// A leaked or inherited value naming some other process must change nothing:
/// the invocation neither arms the watchdog nor breaks.
#[test]
fn a_mismatched_watch_parent_value_is_ignored() {
    Command::cargo_bin("ovm")
        .expect("binary")
        .env("OVM_EXIT_WITH_PARENT", "1")
        .arg("--version")
        .assert()
        .success();
}

/// Garbage in the variable is ignored the same way, not an error.
#[test]
fn a_garbage_watch_parent_value_is_ignored() {
    Command::cargo_bin("ovm")
        .expect("binary")
        .env("OVM_EXIT_WITH_PARENT", "not-a-pid")
        .arg("--version")
        .assert()
        .success();
}

/// The armed case from the child's perspective: the value names its real
/// parent (this test process), the watchdog arms, and a living parent means
/// the command still just runs to completion.
#[test]
fn an_armed_watchdog_with_a_living_parent_changes_nothing() {
    Command::cargo_bin("ovm")
        .expect("binary")
        .env("OVM_EXIT_WITH_PARENT", std::process::id().to_string())
        .arg("--version")
        .assert()
        .success();
}
