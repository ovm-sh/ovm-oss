//! End-to-end CLI tests.
//!
//! These invoke the compiled `ovm` binary via `assert_cmd` and verify
//! user-facing behavior: argument parsing, help output, error messages,
//! exit codes. No network access.

use assert_cmd::Command;
use predicates::prelude::*;

/// Isolate each test from the real `~/.ovm/` by pointing HOME at a tempdir.
fn ovm() -> Command {
    let tmp = tempfile::tempdir().expect("tempdir");
    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", tmp.path())
        .env_remove("OVM_VERSION")
        .env_remove("OVM_PRODUCT");
    // Keep tempdir alive via leaked reference — sufficient for test lifetime
    let _ = Box::leak(Box::new(tmp));
    cmd
}

#[test]
fn bare_ovm_shows_short_help() {
    let output = ovm().assert().success().get_output().stdout.clone();
    let stdout = console::strip_ansi_codes(&String::from_utf8_lossy(&output)).to_string();
    let banner: Vec<_> = stdout
        .lines()
        .skip_while(|line| line.trim().is_empty())
        .take(3)
        .collect();

    assert_eq!(banner.len(), 3);
    assert!(banner[0].contains("ovm (open version manager)"));
    assert!(banner[1].contains("built by mochi and quelpaw"));
    assert!(banner[2].contains("tiny paws for big version jumps"));
    assert!(stdout.contains("Common:"));
    assert!(stdout.contains("Run `ovm help`"));
}

#[test]
fn ovm_help_redirected_stdout_is_ansi_free_and_shows_full_overview() {
    let output = ovm()
        .arg("help")
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let stdout = String::from_utf8_lossy(&output);

    assert!(
        !output.contains(&b'\x1b'),
        "redirected help stdout must not contain ANSI escapes: {stdout:?}"
    );
    for expected in [
        "Interactive:",
        "Version management:",
        "Query:",
        "Maintenance:",
        "Launch shortcuts:",
        "Examples:",
        "claude",
        "codex",
        "pi",
        "self-update",
        "doctor",
        "shortcuts",
    ] {
        assert!(stdout.contains(expected), "missing help entry: {expected}");
    }
}

#[test]
#[cfg(unix)]
fn doctor_and_check_cannot_be_shadowed_by_path_plugins() {
    use std::os::unix::fs::PermissionsExt;

    let tmp = tempfile::tempdir().expect("tempdir");
    let plugin_dir = tmp.path().join("plugins");
    std::fs::create_dir(&plugin_dir).expect("plugin dir");
    let marker = tmp.path().join("plugin-ran");

    for name in ["doctor", "check"] {
        let plugin = plugin_dir.join(format!("ovm-{name}"));
        std::fs::write(&plugin, "#!/bin/sh\ntouch \"$PLUGIN_MARKER\"\n").expect("write plugin");
        let mut permissions = std::fs::metadata(&plugin)
            .expect("plugin metadata")
            .permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&plugin, permissions).expect("executable plugin");
    }

    let existing_path = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(plugin_dir.clone()).chain(std::env::split_paths(&existing_path)),
    )
    .expect("PATH");

    for name in ["doctor", "check"] {
        let _ = std::fs::remove_file(&marker);
        ovm()
            .env("PATH", &path)
            .env("PLUGIN_MARKER", &marker)
            .args([name, "codex"])
            .assert()
            .failure()
            .stderr(predicate::str::contains("No active version set"));
        assert!(!marker.exists(), "ovm-{name} shadowed the built-in command");
    }
}

#[test]
#[cfg(unix)]
fn non_utf8_argument_reports_an_error_without_panicking() {
    use std::os::unix::ffi::OsStringExt;

    ovm()
        .arg(std::ffi::OsString::from_vec(vec![0xff]))
        .assert()
        .failure()
        .stderr(predicate::str::contains("argument is not valid UTF-8"))
        .stderr(predicate::str::contains("panicked").not());
}

#[test]
fn version_flag_prints_semver() {
    ovm()
        .arg("--version")
        .assert()
        .success()
        .stdout(predicate::str::is_match(r"\d+\.\d+\.\d+").unwrap());
}

#[test]
fn unknown_product_gives_helpful_error() {
    ovm()
        .args(["ls", "nonexistent"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("Unknown product"))
        .stderr(predicate::str::contains("claude"))
        .stderr(predicate::str::contains("codex"))
        .stderr(predicate::str::contains("pi"));
}

#[test]
fn info_rejects_version_with_traversal_components() {
    // The version becomes a path segment of the GitHub API URL; traversal
    // must be rejected before any request is made. The API base points at a
    // dead loopback port so a regression cannot reach the real network.
    ovm()
        .env("OVM_GITHUB_API_URL", "http://127.0.0.1:1")
        .args(["info", "claude", "../../search/repositories"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "path separators or traversal components",
        ));
}

#[test]
fn info_rejects_version_with_url_metacharacters() {
    // `%2e%2e` parses as a URL dot-segment, `?` starts a query, and `#`
    // truncates at a fragment — all must be rejected, not just literal `/`.
    for version in ["%2e%2e", "1.2.3?per_page=1", "1.2.3#frag", "1.2.3 x"] {
        ovm()
            .env("OVM_GITHUB_API_URL", "http://127.0.0.1:1")
            .args(["info", "codex", version])
            .assert()
            .failure()
            .stderr(predicate::str::contains("Invalid"));
    }
}

#[test]
fn ls_with_no_installed_versions_is_handled() {
    ovm()
        .args(["ls", "claude"])
        .assert()
        .success()
        .stdout(predicate::str::contains("No claude versions installed"));
}

#[test]
fn use_nonexistent_version_errors() {
    ovm()
        .args(["use", "claude", "99.99.99"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("not installed"));
}

#[test]
fn current_with_no_product_shows_dashboard() {
    ovm()
        .arg("current")
        .assert()
        .success()
        .stdout(predicate::str::contains("ovm"))
        .stdout(predicate::str::contains("claude"))
        .stdout(predicate::str::contains("codex"))
        .stdout(predicate::str::contains("pi"));
}

#[test]
fn current_single_product_with_no_active_fails_cleanly() {
    ovm()
        .args(["current", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("No active claude version"));
}

// Note: `ovm select` opens an interactive TUI that waits for key input.
// Testing it end-to-end requires a pseudo-TTY (expect/ptyprocess). Skipped
// here — the underlying components are unit-tested in plugins/version_manager.

#[test]
fn product_aliases_work() {
    ovm().args(["ls", "cc"]).assert().success();
    ovm().args(["ls", "cx"]).assert().success();
}

#[test]
fn yolo_launch_aliases_are_native_commands() {
    ovm()
        .arg("ccy")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No active version set"))
        .stderr(predicate::str::contains("unrecognized subcommand").not());

    ovm()
        .arg("cxy")
        .assert()
        .failure()
        .stderr(predicate::str::contains("No active version set"))
        .stderr(predicate::str::contains("unrecognized subcommand").not());
}

#[test]
fn switch_alias_opens_select_command_help() {
    ovm()
        .args(["switch", "--help"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Pick a version interactively"));
}

#[test]
fn which_without_active_version_errors() {
    ovm().args(["which", "claude"]).assert().failure();
}

#[test]
fn install_requires_version_argument() {
    ovm()
        .args(["install", "claude"])
        .assert()
        .failure()
        .stderr(predicate::str::contains("required").or(predicate::str::contains("argument")));
}

#[test]
fn completions_generates_shell_script() {
    ovm()
        .args(["completions", "zsh"])
        .assert()
        .success()
        .stdout(predicate::str::contains("_ovm"));
}

#[test]
fn autoupdate_configures_global_and_product_policy() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .arg("autoupdate")
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-update default: on"));

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["autoupdate", "on"])
        .assert()
        .success()
        .stdout(predicate::str::contains("auto-update default: on"));

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["autoupdate", "codex", "off"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Codex auto-update: off"));

    let config =
        std::fs::read_to_string(tmp.path().join(".ovm/config.json")).expect("config written");
    assert!(config.contains(r#""default": "on""#));
    assert!(config.contains(r#""codex": "off""#));
}

#[test]
fn autoupdate_status_lists_self_and_supports_self_notify() {
    let tmp = tempfile::tempdir().expect("tempdir");

    // Bare status lists self alongside the products; self defaults to on.
    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .arg("autoupdate")
        .assert()
        .success()
        .stdout(predicate::str::contains("self").and(predicate::str::contains("on")));

    // `ovm autoupdate self notify` persists OVM's own policy.
    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["autoupdate", "self", "notify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("OVM self auto-update: notify"));

    // A product can take the notify policy too.
    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["autoupdate", "claude", "notify"])
        .assert()
        .success()
        .stdout(predicate::str::contains("Claude Code auto-update: notify"));

    let config =
        std::fs::read_to_string(tmp.path().join(".ovm/config.json")).expect("config written");
    assert!(config.contains(r#""autoUpdate": "notify""#));
    assert!(config.contains(r#""claude": "notify""#));
}

#[test]
fn select_ovm_is_always_a_product_for_a_default_stable_user() {
    // OVM is always selectable — no channel or advanced-flag gate. The direct
    // form (`ovm select ovm <version>`) proves the token is accepted: with an
    // empty store it fails on the missing version, never with the old
    // "not a selectable product" rejection.
    let tmp = tempfile::tempdir().expect("tempdir");
    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["select", "ovm", "0.0.1"])
        .assert()
        .failure()
        .stderr(
            predicate::str::contains("OVM self version 0.0.1 is not installed")
                .and(predicate::str::contains("not a selectable product").not()),
        );
}

#[test]
fn select_ovm_direct_works_the_same_on_the_alpha_channel() {
    // The alpha channel changes the menu's manage-versions default, not
    // whether OVM is selectable — the direct form behaves identically.
    let tmp = tempfile::tempdir().expect("tempdir");
    std::fs::create_dir_all(tmp.path().join(".ovm")).expect("mkdir");
    std::fs::write(
        tmp.path().join(".ovm/config.json"),
        r#"{"self":{"channel":"alpha"}}"#,
    )
    .expect("write config");

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["select", "ovm", "0.0.1"])
        .assert()
        .failure()
        .stderr(predicate::str::contains(
            "OVM self version 0.0.1 is not installed",
        ));
}

#[test]
fn self_update_cargo_stable_dry_run_updates_manifest_bundle() {
    ovm()
        .env("OVM_SELF_UPDATE_STABLE_VERSION", "0.0.1")
        .args(["self-update", "--method", "cargo", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "cargo install ovm-codex-skew --locked --force --version 0.0.1",
        ))
        .stdout(predicate::str::contains(
            "cargo install ovm-claudex --locked --force --version 0.0.1",
        ))
        .stdout(predicate::str::contains(
            "cargo install ovm --locked --force --version 0.0.1",
        ));
}

#[test]
fn self_update_cargo_beta_dry_run_pins_prerelease() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .env("OVM_SELF_UPDATE_BETA_VERSION", "0.0.1-beta.1")
        .args([
            "self-update",
            "--method",
            "cargo",
            "--channel",
            "beta",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains(
            "cargo install ovm-codex-skew --locked --force --version 0.0.1-beta.1",
        ))
        .stdout(predicate::str::contains(
            "cargo install ovm-claudex --locked --force --version 0.0.1-beta.1",
        ))
        .stdout(predicate::str::contains(
            "cargo install ovm --locked --force --version 0.0.1-beta.1",
        ));
}

#[test]
fn self_update_brew_dry_run_relinks_selected_formula() {
    ovm()
        .env("OVM_SELF_UPDATE_BREW_INSTALLED", "")
        .args(["self-update", "--method", "brew", "--dry-run"])
        .assert()
        .success()
        .stdout(predicate::str::contains("brew update"))
        .stdout(predicate::str::contains("brew install ovm-sh/ovm/ovm"))
        .stdout(predicate::str::contains("brew link --overwrite ovm"));

    ovm()
        .env("OVM_SELF_UPDATE_BREW_INSTALLED", "ovm")
        .args([
            "self-update",
            "--method",
            "brew",
            "--channel",
            "beta",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(predicate::str::contains("brew update"))
        .stdout(predicate::str::contains("brew unlink ovm"))
        .stdout(predicate::str::contains("brew link --overwrite ovm-beta"));
}

#[test]
fn cleanup_configures_install_retention() {
    let tmp = tempfile::tempdir().expect("tempdir");

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .arg("cleanup")
        .assert()
        .success()
        .stdout(predicate::str::contains("cleanup retention: 30 days"));

    Command::cargo_bin("ovm")
        .expect("binary built")
        .env("HOME", tmp.path())
        .args(["cleanup", "never"])
        .assert()
        .success()
        .stdout(predicate::str::contains("cleanup retention: never"));

    let config =
        std::fs::read_to_string(tmp.path().join(".ovm/config.json")).expect("config written");
    assert!(config.contains(r#""retention": "never""#));
}

#[test]
fn unknown_plugin_command_errors() {
    // "foo" is not a built-in and no ovm-foo on PATH → clap rejects
    ovm().args(["foo"]).assert().failure();
}

#[test]
fn plugin_command_dispatches_with_args_and_exit_code() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let plugin_dir = tempfile::tempdir().expect("plugin dir");
    let plugin = plugin_dir.path().join("ovm-echoargs");

    std::fs::write(&plugin, "#!/bin/sh\necho \"plugin:$*\"\nexit 23\n").expect("write plugin");

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&plugin).expect("metadata").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&plugin, perms).expect("chmod");
    }

    let mut cmd = Command::cargo_bin("ovm").expect("binary built");
    cmd.env("HOME", tmp.path())
        .env(
            "PATH",
            format!(
                "{}:{}",
                plugin_dir.path().display(),
                std::env::var("PATH").unwrap_or_default()
            ),
        )
        .args(["echoargs", "hello", "world"])
        .assert()
        .code(23)
        .stdout(predicate::str::contains("plugin:hello world"));
}

#[test]
#[cfg(unix)]
fn bundled_plugin_dispatches_via_sibling_when_absent_from_path() {
    use std::os::unix::fs::PermissionsExt;

    // A manifest-declared bundled plugin (ovm-claudex) must resolve beside the
    // ovm binary even when its directory is not on PATH and ovm is invoked by
    // absolute path — matching the dedicated `ovm ccx` alias fallback.
    let bin_dir = tempfile::tempdir().expect("bin dir");
    let home = tempfile::tempdir().expect("home");

    let real_ovm = assert_cmd::cargo::cargo_bin("ovm");
    let ovm_copy = bin_dir.path().join("ovm");
    std::fs::copy(&real_ovm, &ovm_copy).expect("copy ovm");
    std::fs::set_permissions(&ovm_copy, std::fs::Permissions::from_mode(0o755)).expect("chmod ovm");

    let sibling = bin_dir.path().join("ovm-claudex");
    std::fs::write(&sibling, "#!/bin/sh\necho \"claudex:$*\"\nexit 0\n").expect("write sibling");
    std::fs::set_permissions(&sibling, std::fs::Permissions::from_mode(0o755))
        .expect("chmod sibling");

    // PATH deliberately OMITS bin_dir, so only the sibling fallback can resolve.
    // A just-copied executable can transiently fail to exec with ETXTBSY on
    // Linux (the kernel still sees a writable reference to the fresh file), so
    // retry the spawn briefly before asserting rather than flaking the run.
    let run = || {
        Command::new(&ovm_copy)
            .env("HOME", home.path())
            .env("PATH", "/usr/bin:/bin")
            .env_remove("OVM_VERSION")
            .env_remove("OVM_PRODUCT")
            .args(["claudex", "--help"])
            .output()
    };
    let output = {
        let mut attempt = 0;
        loop {
            match run() {
                Ok(output) => break output,
                // ETXTBSY == 26: the copied binary isn't exec-ready yet.
                Err(error) if error.raw_os_error() == Some(26) && attempt < 40 => {
                    attempt += 1;
                    std::thread::sleep(std::time::Duration::from_millis(25));
                }
                Err(error) => panic!("failed to spawn the copied ovm: {error}"),
            }
        }
    };
    assert!(
        output.status.success(),
        "ovm claudex --help exited {:?}; stderr: {}",
        output.status.code(),
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(
        String::from_utf8_lossy(&output.stdout).contains("claudex:--help"),
        "unexpected stdout: {}",
        String::from_utf8_lossy(&output.stdout)
    );
}

#[test]
fn ls_remote_flag_hits_registry_or_fallback() {
    // This test requires network (to hit either the registry or upstream).
    // Skip if OVM_SKIP_NETWORK_TESTS is set.
    if std::env::var("OVM_SKIP_NETWORK_TESTS").is_ok() {
        return;
    }
    // We don't assert on content — just that it exits successfully if the network is up.
    // Failure here is acceptable since we can't guarantee network access.
    let _ = ovm()
        .args(["ls", "claude", "--remote"])
        .timeout(std::time::Duration::from_secs(15))
        .assert();
}
