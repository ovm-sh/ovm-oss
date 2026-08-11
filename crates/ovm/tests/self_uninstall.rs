//! End-to-end coverage for `ovm self uninstall`.
//!
//! Every test drives the real binary against a throwaway `$HOME`, so the shell
//! profiles being rewritten and the tree being deleted are always the tempdir's.
//! `ZDOTDIR` is cleared explicitly: it is inherited from the developer running
//! `cargo test`, and honouring it here would send profile edits at their real
//! zsh config.

#![cfg(unix)]

use assert_cmd::Command;
use predicates::prelude::*;
use std::fs;
use std::os::unix::fs::{symlink, PermissionsExt};
use std::path::Path;
use tempfile::{tempdir, TempDir};

const MANIFEST: &str = "ovm-bundle-v1\nmain\tovm\tovm\nside\tovm-claudex\tovm-claudex\n";

/// The block `install.sh` appends, byte for byte (leading blank line included).
const PATH_BLOCK: &str = "\n# >>> ovm >>>\nexport PATH=\"$HOME/.ovm/bin:$PATH\"\n# <<< ovm <<<\n";

fn ovm(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", home)
        .env("NO_COLOR", "1")
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env_remove("ZDOTDIR")
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    cmd
}

/// A tree shaped like a real direct install: control plane and shims in
/// `~/.ovm/bin`, an immutable snapshot under `~/.ovm/self`, one installed
/// product, a config file, and a foreign file that OVM never wrote.
fn installed() -> TempDir {
    let temp = tempdir().expect("tempdir");
    let home = temp.path();

    let bin = home.join(".ovm/bin");
    fs::create_dir_all(&bin).unwrap();
    fs::write(bin.join("ovm"), b"#!/bin/sh\n# control plane\n").unwrap();
    symlink("ovm", bin.join("ovm-claudex")).unwrap();
    symlink("ovm", bin.join("codex")).unwrap();
    fs::write(bin.join("not-ovms"), b"a file the user put here\n").unwrap();

    let self_root = home.join(".ovm/self");
    let version_dir = self_root.join("versions/0.1.2");
    fs::create_dir_all(&version_dir).unwrap();
    fs::write(version_dir.join("ovm-bundle-v1.tsv"), MANIFEST).unwrap();
    fs::write(version_dir.join("ovm"), b"#!/bin/sh\n").unwrap();
    fs::write(version_dir.join("ovm-claudex"), b"#!/bin/sh\n").unwrap();
    fs::write(version_dir.join(".complete"), b"").unwrap();
    symlink(&version_dir, self_root.join("current")).unwrap();
    fs::write(self_root.join("side-links"), "ovm-claudex\n").unwrap();
    fs::write(
        self_root.join("launcher-dir"),
        format!("{}\n", bin.display()),
    )
    .unwrap();

    let product = home.join(".ovm/products/codex/versions/1.2.3");
    fs::create_dir_all(&product).unwrap();
    fs::write(product.join("codex"), b"#!/bin/sh\n").unwrap();
    fs::write(home.join(".ovm/config.json"), "{}\n").unwrap();

    // claudex's isolated Claude home — the user's claudex conversations live
    // here and nowhere else, so what happens to it must be spelled out.
    let claudex_sessions = home.join(".ovm/claudex/claude/projects");
    fs::create_dir_all(&claudex_sessions).unwrap();
    fs::write(claudex_sessions.join("session.jsonl"), "{}\n").unwrap();

    temp
}

fn read(path: &Path) -> String {
    fs::read_to_string(path).unwrap()
}

#[test]
fn removes_the_installer_path_block_and_nothing_else_in_the_profile() {
    let temp = installed();
    let home = temp.path();

    let zshrc = home.join(".zshrc");
    fs::write(
        &zshrc,
        format!("export EDITOR=vim\n{PATH_BLOCK}alias ll='ls -l'\n"),
    )
    .unwrap();
    // A PATH line the user wrote themselves — same directory, no markers. It is
    // not ours, so it must survive untouched.
    let profile = home.join(".profile");
    let hand_written = "export PATH=\"$HOME/.ovm/bin:$PATH\"\n";
    fs::write(&profile, hand_written).unwrap();
    // The zsh login file the installer also writes to.
    let zprofile = home.join(".zprofile");
    fs::write(&zprofile, format!("# login\n{PATH_BLOCK}")).unwrap();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .success();

    assert_eq!(read(&zshrc), "export EDITOR=vim\nalias ll='ls -l'\n");
    assert_eq!(read(&zprofile), "# login\n");
    assert_eq!(read(&profile), hand_written);
}

#[test]
fn a_profile_without_the_block_is_left_alone_and_still_succeeds() {
    let temp = installed();
    let home = temp.path();

    let zshrc = home.join(".zshrc");
    let untouched = "export EDITOR=vim\nalias ll='ls -l'\n";
    fs::write(&zshrc, untouched).unwrap();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .success();

    assert_eq!(read(&zshrc), untouched);
    // No block anywhere is not "nothing was installed": the tree still went.
    assert!(!home.join(".ovm/self").exists());
}

#[test]
fn default_uninstall_keeps_product_installs_and_config() {
    let temp = installed();
    let home = temp.path();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Removed:"));

    // OVM's own footprint is gone.
    assert!(!home.join(".ovm/self").exists());
    assert!(!home.join(".ovm/bin/ovm").exists());
    assert!(!home.join(".ovm/bin/ovm-claudex").is_symlink());
    assert!(!home.join(".ovm/bin/codex").is_symlink());

    // Everything that is not OVM stays.
    assert!(home
        .join(".ovm/products/codex/versions/1.2.3/codex")
        .exists());
    assert_eq!(read(&home.join(".ovm/config.json")), "{}\n");
    assert_eq!(
        read(&home.join(".ovm/bin/not-ovms")),
        "a file the user put here\n"
    );
}

#[test]
fn purge_removes_the_whole_ovm_tree() {
    let temp = installed();
    let home = temp.path();

    ovm(home)
        .args(["self", "uninstall", "--purge", "--yes"])
        .assert()
        .success();

    assert!(!home.join(".ovm").exists());
}

#[test]
fn the_purge_preview_spells_out_that_claudex_history_goes_too() {
    let temp = installed();
    let home = temp.path();

    ovm(home)
        .args(["self", "uninstall", "--purge", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("~/.ovm/claudex"))
        .stdout(predicate::str::contains("claudex session"));

    assert!(!home.join(".ovm/claudex").exists());
}

#[test]
fn a_default_uninstall_says_claudex_history_stays_and_keeps_it() {
    let temp = installed();
    let home = temp.path();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "~/.ovm/claudex (claudex sessions, history, and proxy auth)",
        ));

    assert_eq!(
        read(&home.join(".ovm/claudex/claude/projects/session.jsonl")),
        "{}\n"
    );
}

/// Recursive uninstall never follows a symlinked OVM root. A wrong link can
/// otherwise turn `--purge` into deletion of an unrelated directory.
#[test]
fn a_symlinked_ovm_home_is_refused_without_touching_the_target() {
    let temp = installed();
    let home = temp.path();

    // Move the whole tree aside and leave a symlink where it was, the shape of
    // an install kept on external storage.
    let external = home.join("external-storage/ovm-state");
    fs::create_dir_all(external.parent().unwrap()).unwrap();
    fs::rename(home.join(".ovm"), &external).unwrap();
    symlink(&external, home.join(".ovm")).unwrap();

    ovm(home)
        .args(["self", "uninstall", "--purge", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Refusing to uninstall through symlinked",
        ));

    assert!(
        external.exists(),
        "the symlink target must remain untouched"
    );
    assert!(home.join(".ovm").is_symlink());
}

/// The same install, without `--purge`: OVM's own footprint goes, the product
/// versions on the other disk stay.
#[test]
fn a_symlinked_ovm_home_default_uninstall_is_also_refused() {
    let temp = installed();
    let home = temp.path();

    let external = home.join("external-storage/ovm-state");
    fs::create_dir_all(external.parent().unwrap()).unwrap();
    fs::rename(home.join(".ovm"), &external).unwrap();
    symlink(&external, home.join(".ovm")).unwrap();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "Refusing to uninstall through symlinked",
        ));

    assert!(external.join("self").exists());
    assert!(external.join("bin/ovm").exists());
    assert!(external
        .join("products/codex/versions/1.2.3/codex")
        .exists());
    assert!(home.join(".ovm").is_symlink());
}

/// A `~/.ovm/self` symlinked at somebody else's directory used to be walked
/// straight into by `remove_dir_all`.
#[test]
fn a_self_directory_symlinked_out_of_the_tree_is_refused() {
    let temp = installed();
    let home = temp.path();

    let foreign = home.join("Documents/thesis");
    fs::create_dir_all(&foreign).unwrap();
    fs::write(foreign.join("chapter-1.md"), "years of work\n").unwrap();
    fs::remove_dir_all(home.join(".ovm/self")).unwrap();
    symlink(&foreign, home.join(".ovm/self")).unwrap();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .failure()
        .stdout(predicate::str::contains(
            "is a symlink; refusing recursive deletion",
        ));

    assert_eq!(read(&foreign.join("chapter-1.md")), "years of work\n");
}

/// The recorded launcher directory is an absolute path OVM reads from a file.
/// If it points somewhere else — a stale record, a hand-edited one — the `ovm`
/// found there belongs to whoever put it there.
#[test]
fn an_ovm_at_the_recorded_path_that_ovm_did_not_write_is_left_alone() {
    let temp = installed();
    let home = temp.path();

    let foreign_dir = home.join("usr/local/bin");
    fs::create_dir_all(&foreign_dir).unwrap();
    let foreign = foreign_dir.join("ovm");
    let contents = "#!/bin/sh\n# somebody else's ovm\n";
    fs::write(&foreign, contents).unwrap();
    fs::write(
        home.join(".ovm/self/launcher-dir"),
        format!("{}\n", foreign_dir.display()),
    )
    .unwrap();

    ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("not OVM's, left in place"));

    assert_eq!(read(&foreign), contents);
    // The rest of the uninstall still ran.
    assert!(!home.join(".ovm/self").exists());
}

/// An unsearchable launcher directory is the shape of the bug this guards:
/// every probe of `~/.ovm/bin/<name>` fails with EACCES, which used to read as
/// "not installed" and produced a green "OVM uninstalled" while the shims were
/// still there. It must fail loudly instead.
#[test]
fn a_launcher_directory_it_cannot_search_is_a_failure_not_a_clean_run() {
    let temp = installed();
    let home = temp.path();
    let bin = home.join(".ovm/bin");

    let sealed = fs::Permissions::from_mode(0o000);
    let original = fs::metadata(&bin).unwrap().permissions();
    fs::set_permissions(&bin, sealed).unwrap();
    if fs::read_dir(&bin).is_ok() {
        // Running as root (or on a filesystem that ignores the mode): the
        // precondition this test needs does not exist here.
        fs::set_permissions(&bin, original).unwrap();
        return;
    }

    let assertion = ovm(home)
        .args(["self", "uninstall", "--yes"])
        .assert()
        .failure()
        .stdout(predicate::str::contains("could not be inspected"));

    fs::set_permissions(&bin, original).unwrap();
    // The shims really are still there — the non-zero exit was the truth.
    assert!(home.join(".ovm/bin/ovm").exists());
    assert!(home.join(".ovm/bin/ovm-claudex").is_symlink());
    drop(assertion);
}

#[test]
fn non_interactive_without_yes_refuses_and_changes_nothing() {
    let temp = installed();
    let home = temp.path();

    let zshrc = home.join(".zshrc");
    let before = format!("export EDITOR=vim\n{PATH_BLOCK}");
    fs::write(&zshrc, &before).unwrap();

    ovm(home)
        .args(["self", "uninstall"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not interactive"))
        .stderr(predicate::str::contains("--yes"));

    assert_eq!(read(&zshrc), before);
    assert!(home.join(".ovm/self/current").exists());
    assert!(home.join(".ovm/bin/ovm").exists());
    assert!(home.join(".ovm/bin/ovm-claudex").is_symlink());
}

#[test]
fn uninstalling_twice_is_clean_rather_than_an_error() {
    let temp = installed();
    let home = temp.path();

    ovm(home)
        .args(["self", "uninstall", "--purge", "--yes"])
        .assert()
        .success();
    ovm(home)
        .args(["self", "uninstall", "--purge", "--yes"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Nothing left to remove"));
}
