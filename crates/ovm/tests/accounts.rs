//! `ovm accounts` and `ovm run`: account folders that share one brain.
//!
//! Every test runs against a throwaway HOME; nothing here may touch a real
//! `~/.claude` or `~/.claude.json`.

use assert_cmd::Command;
use predicates::prelude::*;
use serde_json::Value;
use std::fs;
use std::path::Path;

fn home() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join(".claude/projects")).unwrap();
    fs::write(root.join(".claude/CLAUDE.md"), "# me\n").unwrap();
    fs::write(root.join(".claude/policy-limits.json"), "{}").unwrap();
    fs::write(
        root.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"main-login","emailAddress":"me@example.com"},"mcpServers":{"n":{"command":"x"}}}"#,
    )
    .unwrap();
    temp
}

fn ovm(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ovm").unwrap();
    cmd.env("HOME", home)
        .env("OVM_NO_SELF_UPDATE", "1")
        .env("OVM_QUIET", "1")
        .env_remove("CLAUDE_CONFIG_DIR")
        .env_remove("OVM_ACCOUNT");
    cmd
}

#[test]
fn add_makes_a_folder_that_shares_the_brain_and_not_the_login() {
    let home = home();
    ovm(home.path())
        .args(["accounts", "add", "simcity"])
        .assert()
        .success()
        .stderr(predicate::str::contains("sign it in once"));
    let folder = home.path().join(".claude-accounts/simcity");
    assert!(fs::symlink_metadata(folder.join("projects"))
        .unwrap()
        .file_type()
        .is_symlink());
    assert!(!folder.join("policy-limits.json").exists());
    let own: Value =
        serde_json::from_str(&fs::read_to_string(folder.join(".claude.json")).unwrap()).unwrap();
    assert!(own.get("oauthAccount").is_none(), "{own}");
    let main: Value =
        serde_json::from_str(&fs::read_to_string(home.path().join(".claude.json")).unwrap())
            .unwrap();
    assert_eq!(main["oauthAccount"]["accountUuid"], "main-login");

    ovm(home.path())
        .args(["accounts", "list"])
        .assert()
        .success()
        .stdout(predicate::str::contains("simcity"))
        .stdout(predicate::str::contains("not signed in"))
        .stdout(predicate::str::contains("me@example.com"));
}

#[test]
fn bind_and_default_are_recorded_and_bad_names_are_refused() {
    let home = home();
    let client = home.path().join("work/client");
    fs::create_dir_all(&client).unwrap();
    ovm(home.path())
        .args(["accounts", "add", "team-a", "--team"])
        .assert()
        .success();
    ovm(home.path())
        .args(["accounts", "bind"])
        .arg(&client)
        .arg("team-a")
        .assert()
        .success();
    ovm(home.path())
        .args(["accounts", "default", "team-a"])
        .assert()
        .success();
    let registry: Value =
        serde_json::from_str(&fs::read_to_string(home.path().join(".ovm/accounts.json")).unwrap())
            .unwrap();
    assert_eq!(registry["accounts"]["team-a"]["kind"], "team");
    assert_eq!(registry["bindings"][0]["account"], "team-a");
    assert_eq!(registry["default"], "team-a");

    ovm(home.path())
        .args(["accounts", "bind"])
        .arg(&client)
        .arg("nobody")
        .assert()
        .failure()
        .stderr(predicate::str::contains("no account `nobody`"));
    ovm(home.path())
        .args(["accounts", "add", "../escape"])
        .assert()
        .failure();
}

#[test]
fn run_names_a_missing_account_instead_of_launching_as_someone_else() {
    let home = home();
    ovm(home.path())
        .args(["run", "simcity"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("no account called `simcity`"));
}

#[test]
fn a_signed_in_folder_is_handed_to_limits_as_live_only() {
    use std::os::unix::fs::PermissionsExt;
    let home = home();
    ovm(home.path())
        .args(["accounts", "add", "client", "--team"])
        .assert()
        .success();
    let folder = home.path().join(".claude-accounts/client");
    fs::write(
        folder.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"seat-login","organizationType":"claude_team"}}"#,
    )
    .unwrap();
    // A stand-in `ovm-limits` that records what it was asked.
    let bin = home.path().join("bin");
    fs::create_dir_all(&bin).unwrap();
    let record = home.path().join("limits-args");
    let fake = bin.join("ovm-limits");
    fs::write(
        &fake,
        format!("#!/bin/sh\necho \"$@\" > '{}'\n", record.display()),
    )
    .unwrap();
    fs::set_permissions(&fake, fs::Permissions::from_mode(0o755)).unwrap();
    let path = format!("{}:/usr/bin:/bin", bin.display());
    // No Claude Code is installed in this HOME and none can be fetched, so
    // the launch itself fails after the hand-over, which is what is tested.
    let _ = ovm(home.path())
        .env("PATH", path)
        .env("OVM_ALLOW_PLUGIN_OVERRIDE", "1")
        // Nothing to download from: the launch fails fast and offline.
        .env("OVM_CLAUDE_CDN_URL", "http://127.0.0.1:9")
        .env("OVM_REGISTRY_BASE_URL", "http://127.0.0.1:9")
        .args(["run", "client", "--version"])
        .assert();
    let args = fs::read_to_string(&record).expect("ovm-limits was not called");
    assert!(
        args.starts_with("add claude client --live-only --no-login --home "),
        "{args}"
    );
    assert!(args.contains(".claude-accounts/client"), "{args}");
    assert!(args.trim_end().ends_with("--team"), "{args}");
}

#[test]
fn a_plain_claude_launch_in_a_bound_directory_runs_as_the_bound_account() {
    let home = home();
    ovm(home.path())
        .args(["accounts", "add", "client"])
        .assert()
        .success();
    let repo = home.path().join("work/client-repo");
    let elsewhere = home.path().join("work/other");
    fs::create_dir_all(repo.join("src")).unwrap();
    fs::create_dir_all(&elsewhere).unwrap();
    ovm(home.path())
        .args(["accounts", "bind"])
        .arg(&repo)
        .arg("client")
        .assert()
        .success();
    ovm(home.path())
        .args(["accounts", "default", "client"])
        .assert()
        .success();
    let launch = |cwd: &Path, config_dir: Option<&Path>| {
        let mut cmd = ovm(home.path());
        cmd.current_dir(cwd)
            .env("PATH", "/usr/bin:/bin")
            .env("OVM_CLAUDE_CDN_URL", "http://127.0.0.1:9")
            .env("OVM_REGISTRY_BASE_URL", "http://127.0.0.1:9")
            .args(["claude", "--version"]);
        if let Some(dir) = config_dir {
            cmd.env("CLAUDE_CONFIG_DIR", dir);
        }
        let output = cmd.output().unwrap();
        String::from_utf8_lossy(&output.stderr).into_owned()
    };
    // Below the bound directory: the bound account.
    let bound = launch(&repo.join("src"), None);
    assert!(bound.contains("client is not signed in yet"), "{bound}");
    // Outside every binding the default does not apply to a plain launch.
    let other = launch(&elsewhere, None);
    assert!(!other.contains("client"), "{other}");
    // A home someone chose (a limits poll, `ovm run`) is never overridden.
    let chosen = launch(&repo, Some(&home.path().join("chosen")));
    assert!(!chosen.contains("client"), "{chosen}");
}
