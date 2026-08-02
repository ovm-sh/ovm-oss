//! Retention-cleanup safety on the launch path.
//!
//! A bare `ovm <product>` (no explicit version) runs retention maintenance
//! before it execs. That maintenance once called a real `uninstall` on every
//! aged-out install it found, silently, with no cap: one launch in a fresh
//! directory removed 209 installed versions and 32.4 GB, reported only
//! afterwards and only as "Cleaned up 209 old install(s)".
//!
//! These tests pin the properties that would have caught it:
//!   - a large backlog is never acted on unattended — the launch reports it and
//!     leaves every install in place,
//!   - a small backlog is named *before* it is touched, and is archived
//!     (reversible) rather than deleted,
//!   - installs inside the retention window are never touched at all,
//!   - the explicit destructive path (`ovm cleanup now`) lists everything first
//!     and then needs a real answer: it refuses outright without a terminal, and
//!     under a PTY a bare Enter means no.

use assert_cmd::Command;
use filetime::{set_file_mtime, FileTime};
use rexpect::spawn;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

/// Retention window used by every test here.
const RETENTION_DAYS: u64 = 30;

/// A URL on a closed port: every request fails instantly, so no test in this
/// file can reach (or wait on) the network.
fn dead_port_url() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind for free port");
    let addr = listener.local_addr().expect("dead-port addr");
    drop(listener);
    format!("http://{addr}")
}

fn ovm(home: &Path) -> Command {
    let url = dead_port_url();
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_CODEX_RELEASES_URL", &url)
        .env("OVM_REGISTRY_BASE_URL", &url)
        .env("OVM_CODEX_NPM_REGISTRY_URL", &url)
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1");
    cmd
}

/// Config with retention *on* — the setting the incident ran under. Update
/// checks stay off so the launch never leaves the machine.
fn write_config(home: &Path) {
    let config = home.join(".ovm/config.json");
    fs::create_dir_all(config.parent().expect("config parent")).expect("mkdir config parent");
    fs::write(
        config,
        format!(
            r#"{{
                "checkForUpdates": false,
                "autoUpdate": {{ "default": "off" }},
                "cleanup": {{ "retention": "{RETENTION_DAYS}" }}
            }}"#
        ),
    )
    .expect("write test config");
}

fn version_dir(home: &Path, version: &str) -> PathBuf {
    home.join(".ovm/products/codex/versions").join(version)
}

fn codex_binary(home: &Path, version: &str) -> PathBuf {
    version_dir(home, version).join("release/bin/codex")
}

/// Seed a complete Codex install: a runnable shell-script binary plus the
/// completion marker, which is what makes it count as installed.
fn seed_install(home: &Path, version: &str) {
    let binary = codex_binary(home, version);
    fs::create_dir_all(binary.parent().expect("binary parent")).expect("mkdir version dir");
    fs::write(
        &binary,
        format!("#!/bin/sh\necho \"codex {version} args=$*\"\n"),
    )
    .expect("write fake binary");
    let mut perms = fs::metadata(&binary).expect("metadata").permissions();
    perms.set_mode(0o755);
    fs::set_permissions(&binary, perms).expect("chmod");
    fs::write(version_dir(home, version).join("release/.complete"), "")
        .expect("write completion marker");
}

/// Point `current` at a seeded install, the way `ovm use` would.
fn activate(home: &Path, version: &str) {
    let current = home.join(".ovm/products/codex/current");
    let _ = fs::remove_file(&current);
    std::os::unix::fs::symlink(version_dir(home, version), &current).expect("current symlink");
}

fn age_days(home: &Path, version: &str, days: u64) {
    let then = SystemTime::now() - Duration::from_secs(days * 24 * 60 * 60);
    set_file_mtime(version_dir(home, version), FileTime::from_system_time(then))
        .expect("set mtime");
}

/// A home with `active` selected and `aged` installs that all crossed the
/// retention line long ago.
fn home_with_aged_installs(aged: &[&str]) -> tempfile::TempDir {
    let home = tempfile::tempdir().expect("tempdir");
    write_config(home.path());
    seed_install(home.path(), "rust-v0.130.0");
    activate(home.path(), "rust-v0.130.0");
    for version in aged {
        seed_install(home.path(), version);
    }
    // Age the active install too: being current must protect it, not its mtime.
    age_days(home.path(), "rust-v0.130.0", RETENTION_DAYS + 1);
    for version in aged {
        age_days(home.path(), version, RETENTION_DAYS + 1);
    }
    home
}

/// The regression guard for the incident itself. Four aged installs is past the
/// unattended allowance, so a bare launch must exec the editor and touch
/// nothing.
#[test]
fn launch_defers_large_backlog_instead_of_removing_it() {
    let aged = [
        "rust-v0.118.0",
        "rust-v0.119.0",
        "rust-v0.120.0",
        "rust-v0.121.0",
    ];
    let home = home_with_aged_installs(&aged);

    let assert = ovm(home.path())
        .arg("codex")
        .assert()
        .success()
        .stdout(predicates::str::contains("codex rust-v0.130.0 args="));

    for version in aged {
        assert!(
            codex_binary(home.path(), version).exists(),
            "{version} must survive an unattended launch"
        );
    }
    assert!(codex_binary(home.path(), "rust-v0.130.0").exists());

    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8 stderr");
    assert!(
        stderr.contains("4 installed versions"),
        "launch must report the backlog it refused to touch: {stderr}"
    );
    assert!(
        stderr.contains("will not remove this many"),
        "launch must say it declined: {stderr}"
    );
    assert!(
        stderr.contains("ovm cleanup now"),
        "launch must point at the deliberate path: {stderr}"
    );
    assert!(
        !stderr.contains("Cleaned up"),
        "the past-tense cache-clean wording is the bug: {stderr}"
    );
}

/// There is no size at which a launch acts. A small backlog is reported and
/// left exactly where it is.
///
/// An earlier version of this fix archived up to three installs unattended,
/// on the grounds that archiving is reversible. It is not, for every product:
/// archiving Codex or Pi deletes the release tree while the downloaded
/// artifact has already been discarded, so an offline user — or one whose
/// upstream asset has since disappeared — cannot get it back. Planning and
/// acting are also separate steps, so a concurrent `ovm use` could leave
/// `current` pointing at something the launch had just archived.
#[test]
fn launch_reports_a_small_backlog_without_touching_it() {
    let aged = ["rust-v0.118.0", "rust-v0.119.0", "rust-v0.120.0"];
    let home = home_with_aged_installs(&aged);

    let assert = ovm(home.path()).arg("codex").assert().success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8 stderr");

    for version in aged {
        assert!(
            codex_binary(home.path(), version).exists(),
            "{version} must survive a launch: a launch removes nothing"
        );
        assert!(version_dir(home.path(), version).exists());
    }
    assert!(codex_binary(home.path(), "rust-v0.130.0").exists());

    // Silence would be its own bug: the user should learn the backlog exists.
    assert!(
        stderr.contains("unused") || stderr.contains("ovm cleanup"),
        "a backlog must still be reported: {stderr}"
    );
    // And never in the past tense, which is what made 32 GB vanish quietly.
    assert!(
        !stderr.contains("Archived") && !stderr.contains("Cleaned up"),
        "a launch must not report having removed anything: {stderr}"
    );
}

/// Installs inside the retention window are not eligible at all, and a launch
/// with nothing to do says nothing.
#[test]
fn launch_leaves_installs_inside_the_retention_window_alone() {
    let home = tempfile::tempdir().expect("tempdir");
    write_config(home.path());
    seed_install(home.path(), "rust-v0.130.0");
    activate(home.path(), "rust-v0.130.0");
    for version in ["rust-v0.118.0", "rust-v0.119.0"] {
        seed_install(home.path(), version);
        age_days(home.path(), version, 5);
    }

    let assert = ovm(home.path()).arg("codex").assert().success();
    let stderr = String::from_utf8(assert.get_output().stderr.clone()).expect("utf8 stderr");

    for version in ["rust-v0.118.0", "rust-v0.119.0"] {
        assert!(codex_binary(home.path(), version).exists());
    }
    assert!(
        !stderr.contains("Archiv") && !stderr.contains("unused for"),
        "a launch with nothing eligible must stay quiet: {stderr}"
    );
}

/// The deliberate destructive path lists everything first and then needs a
/// human. In CI, a pipe, or a hook there is nobody to ask, so it must refuse
/// rather than assume consent.
#[test]
fn cleanup_now_lists_then_refuses_without_a_terminal() {
    let aged = [
        "rust-v0.118.0",
        "rust-v0.119.0",
        "rust-v0.120.0",
        "rust-v0.121.0",
    ];
    let home = home_with_aged_installs(&aged);

    let assert = ovm(home.path()).args(["cleanup", "now"]).assert().failure();
    let output = assert.get_output();
    let stdout = String::from_utf8(output.stdout.clone()).expect("utf8 stdout");
    let stderr = String::from_utf8(output.stderr.clone()).expect("utf8 stderr");

    for version in aged {
        assert!(
            stdout.contains(version),
            "every version must be listed before the question: {stdout}"
        );
        assert!(
            codex_binary(home.path(), version).exists(),
            "{version} must survive a refused confirmation"
        );
    }
    assert!(
        stderr.contains("not interactive"),
        "refusal must say why: {stderr}"
    );
}

/// Under a real terminal the question is asked, and the default answer is no:
/// a stray Enter must never be read as consent to delete installs.
#[test]
fn cleanup_now_treats_bare_enter_as_no() {
    let aged = [
        "rust-v0.118.0",
        "rust-v0.119.0",
        "rust-v0.120.0",
        "rust-v0.121.0",
    ];
    let home = home_with_aged_installs(&aged);

    let mut session = spawn(&cleanup_now_command(home.path()), Some(10_000)).expect("spawn pty");
    session
        .exp_string("Permanently remove")
        .expect("confirmation prompt");
    session.send_line("").expect("send bare enter");
    session.exp_string("Nothing removed").expect("declined");
    session.exp_eof().expect("clean exit");

    for version in aged {
        assert!(
            codex_binary(home.path(), version).exists(),
            "{version} must survive a bare Enter"
        );
    }
}

/// ...and an explicit yes does remove them, so the deliberate path still works.
#[test]
fn cleanup_now_removes_after_an_explicit_yes() {
    let aged = ["rust-v0.118.0", "rust-v0.119.0"];
    let home = home_with_aged_installs(&aged);

    let mut session = spawn(&cleanup_now_command(home.path()), Some(10_000)).expect("spawn pty");
    session
        .exp_string("Permanently remove")
        .expect("confirmation prompt");
    session.send_line("y").expect("confirm");
    session.exp_string("Removed").expect("removal summary");
    session.exp_eof().expect("clean exit");

    for version in aged {
        assert!(
            !version_dir(home.path(), version).exists(),
            "{version} should be gone after an explicit yes"
        );
    }
    assert!(codex_binary(home.path(), "rust-v0.130.0").exists());
}

/// `ovm cleanup now` under a PTY, with colour off so expectations match plain
/// text and no network URLs since this path never downloads.
fn cleanup_now_command(home: &Path) -> String {
    format!(
        "env HOME={} NO_COLOR=1 OVM_DISABLE_BACKGROUND_REFRESH=1 {} cleanup now",
        home.display(),
        assert_cmd::cargo::cargo_bin("ovm").display()
    )
}

/// The survey a launch runs is not free — it stat-walks every installed version
/// of every product — and its answer is discarded on almost every launch. So it
/// runs at most once per `updateCheckInterval`, and the next launch does no
/// walking at all. What must NOT change: a command the user typed always looks,
/// however fresh the stamp is.
#[test]
fn the_launch_survey_is_periodic_but_explicit_cleanup_always_looks() {
    let aged = [
        "rust-v0.118.0",
        "rust-v0.119.0",
        "rust-v0.120.0",
        "rust-v0.121.0",
    ];
    let home = home_with_aged_installs(&aged);
    let stamp = home.path().join(".ovm/cleanup-checked");

    // First launch: nothing stamped, so it surveys and reports.
    let first = ovm(home.path()).arg("codex").assert().success();
    let first_stderr = String::from_utf8(first.get_output().stderr.clone()).expect("utf8 stderr");
    assert!(
        first_stderr.contains("4 installed versions"),
        "the first launch must survey and report: {first_stderr}"
    );
    assert!(stamp.exists(), "a survey must record that it ran");

    // Second launch, same backlog: the stamp is fresh, so nothing is walked and
    // nothing is said.
    let second = ovm(home.path()).arg("codex").assert().success();
    let second_stderr = String::from_utf8(second.get_output().stderr.clone()).expect("utf8 stderr");
    assert!(
        !second_stderr.contains("installed versions") && !second_stderr.contains("unused for"),
        "a launch inside the survey interval must not re-survey: {second_stderr}"
    );

    // But the explicit command is never gated by that stamp.
    ovm(home.path())
        .arg("cleanup")
        .assert()
        .success()
        .stdout(predicates::str::contains("4 installed versions"));

    // Neither is `ovm cleanup now`, which still lists every candidate.
    let now = ovm(home.path()).args(["cleanup", "now"]).assert().failure();
    let now_stdout = String::from_utf8(now.get_output().stdout.clone()).expect("utf8 stdout");
    for version in aged {
        assert!(
            now_stdout.contains(version),
            "`cleanup now` must list {version} regardless of the launch stamp: {now_stdout}"
        );
    }

    // And an aged stamp puts the launch notice back.
    let aged_stamp = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .expect("epoch")
        .as_secs()
        .saturating_sub(25 * 60 * 60);
    fs::write(&stamp, format!("{aged_stamp}\n")).expect("age stamp");
    let third = ovm(home.path()).arg("codex").assert().success();
    let third_stderr = String::from_utf8(third.get_output().stderr.clone()).expect("utf8 stderr");
    assert!(
        third_stderr.contains("4 installed versions"),
        "a stale stamp must survey again: {third_stderr}"
    );
}

/// The survey records that it ran by writing `~/.ovm/cleanup-checked`. Written
/// with `fs::write`, that record follows a symlink standing at the stamp path
/// and replaces the contents of whatever it aims at — so a link planted there,
/// pointing at any file the user can write, turns an ordinary launch into
/// unrelated-file corruption. The stamp is published by `rename` instead, which
/// replaces a link at the destination rather than following it (the guarantee
/// itself is pinned in `version_manager.rs` by
/// `a_symlink_at_the_publish_target_is_replaced_not_followed`).
#[test]
fn a_launch_replaces_a_symlink_planted_at_the_stamp_instead_of_writing_through_it() {
    let home = home_with_aged_installs(&["rust-v0.118.0", "rust-v0.119.0"]);

    let victim = home.path().join("precious.txt");
    let victim_bytes = b"the file a planted link would aim at";
    fs::write(&victim, victim_bytes).expect("write victim");

    let stamp = home.path().join(".ovm/cleanup-checked");
    fs::create_dir_all(stamp.parent().expect("stamp parent")).expect("mkdir .ovm");
    std::os::unix::fs::symlink(&victim, &stamp).expect("plant the link");

    ovm(home.path())
        .arg("codex")
        .assert()
        .success()
        .stdout(predicates::str::contains("codex rust-v0.130.0 args="));

    assert_eq!(
        fs::read(&victim).expect("the victim"),
        victim_bytes,
        "a launch must not write its stamp through a planted symlink"
    );
    assert!(
        !fs::symlink_metadata(&stamp)
            .expect("stamp")
            .file_type()
            .is_symlink(),
        "the planted link must be gone, replaced by a real stamp file"
    );
    assert!(
        fs::read_to_string(&stamp)
            .expect("stamp")
            .trim()
            .parse::<u64>()
            .is_ok(),
        "the stamp must still be a usable timestamp"
    );
    // No scratch left behind in ~/.ovm either.
    let leftovers: Vec<String> = fs::read_dir(home.path().join(".ovm"))
        .expect("read .ovm")
        .flatten()
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(
        leftovers.is_empty(),
        "temp files left behind: {leftovers:?}"
    );
}

/// `ovm cleanup` with no argument surfaces the backlog the launch path declines
/// to act on, so the state is visible where retention is configured.
#[test]
fn cleanup_reports_pending_backlog() {
    let home = home_with_aged_installs(&["rust-v0.118.0", "rust-v0.119.0", "rust-v0.120.0"]);

    ovm(home.path())
        .arg("cleanup")
        .assert()
        .success()
        .stdout(predicates::str::contains("cleanup retention: 30 days"))
        .stdout(predicates::str::contains("3 installed versions"))
        .stdout(predicates::str::contains("ovm cleanup now"));
}
