//! `ovm run` and the launch-time failover chain: which account a launch lands
//! on, and what environment the exec'd Claude Code sees, per provider.
//!
//! Every test runs against a throwaway HOME with a fake `claude` that prints
//! the environment it was given; nothing here may touch a real `~/.claude`.

use assert_cmd::Command;
use serde_json::{json, Value};
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

/// The account variable from `commands/run.rs` (`ACCOUNT_ENV`).
const ACCOUNT_ENV: &str = "OVM_ACCOUNT";
const FAKE_CLAUDE_VERSION: &str = "2.1.91";
const OFFLINE_URL: &str = "http://127.0.0.1:9";
const SECONDS_PER_MINUTE: u64 = 60;
const MINUTES_PER_DAY: u64 = 24 * 60;

/// Variables the fake Claude echoes back, one `NAME=value` line each.
const DUMPED_VARS: &[&str] = &[
    "CLAUDE_CONFIG_DIR",
    ACCOUNT_ENV,
    "CLAUDE_CODE_USE_AZURE",
    "ANTHROPIC_BASE_URL",
    "ANTHROPIC_API_KEY",
    "CLAUDE_CODE_USE_BEDROCK",
    "AWS_REGION",
    "AWS_PROFILE",
];

/// A HOME with a main `~/.claude`, update checks off, and a fake Claude Code
/// installed as the active version.
fn home() -> tempfile::TempDir {
    let temp = tempfile::tempdir().unwrap();
    let root = temp.path();
    fs::create_dir_all(root.join(".claude/projects")).unwrap();
    fs::write(root.join(".claude/CLAUDE.md"), "# me\n").unwrap();
    fs::write(
        root.join(".claude.json"),
        r#"{"oauthAccount":{"accountUuid":"main-login","emailAddress":"me@example.com"}}"#,
    )
    .unwrap();
    fs::create_dir_all(root.join(".ovm")).unwrap();
    fs::write(
        root.join(".ovm/config.json"),
        r#"{
            "checkForUpdates": false,
            "autoUpdate": { "default": "off" },
            "cleanup": { "retention": "never" }
        }"#,
    )
    .unwrap();
    fs::create_dir_all(root.join("work")).unwrap();
    // A stand-in `ovm-limits` that declines, so a signed-in launch never
    // reaches a real limits plugin.
    let bin = root.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_limits = bin.join("ovm-limits");
    fs::write(&fake_limits, "#!/bin/sh\nexit 1\n").unwrap();
    fs::set_permissions(&fake_limits, fs::Permissions::from_mode(0o755)).unwrap();
    install_fake_claude(root);
    temp
}

fn ovm(home: &Path) -> Command {
    let mut cmd = Command::cargo_bin("ovm").unwrap();
    cmd.env("HOME", home)
        .env("OVM_NO_SELF_UPDATE", "1")
        .env("OVM_QUIET", "1")
        .env("OVM_DISABLE_BACKGROUND_REFRESH", "1")
        .env("OVM_SKIP_SIGNATURE_VERIFY", "1")
        .env("OVM_CLAUDE_CDN_URL", OFFLINE_URL)
        .env("OVM_REGISTRY_BASE_URL", OFFLINE_URL)
        .env("OVM_ALLOW_PLUGIN_OVERRIDE", "1")
        .env(
            "PATH",
            format!("{}:/usr/bin:/bin", home.join("bin").display()),
        )
        .current_dir(home.join("work"));
    for var in DUMPED_VARS {
        cmd.env_remove(var);
    }
    cmd
}

/// A `claude` that prints each variable we care about, then its arguments.
fn install_fake_claude(home: &Path) {
    let mut script = String::from("#!/bin/sh\n");
    for var in DUMPED_VARS {
        script.push_str(&format!("echo \"{var}=${{{var}-<unset>}}\"\n"));
    }
    script.push_str("echo \"args=$*\"\n");
    let binary = home
        .join(".ovm/products/claude/versions")
        .join(FAKE_CLAUDE_VERSION)
        .join("native/claude");
    let dir = binary.parent().unwrap();
    fs::create_dir_all(dir).unwrap();
    fs::write(&binary, script).unwrap();
    fs::write(dir.join(".complete"), "").unwrap();
    fs::set_permissions(&binary, fs::Permissions::from_mode(0o755)).unwrap();
    ovm(home)
        .args(["use", "claude", FAKE_CLAUDE_VERSION])
        .assert()
        .success();
}

fn accounts(home: &Path, args: &[&str]) {
    ovm(home).arg("accounts").args(args).assert().success();
}

fn folder(home: &Path, label: &str) -> PathBuf {
    home.join(".claude-accounts").join(label)
}

/// Gives `label`'s folder a login. Only a signed-in subscription account's
/// launch line names why it was picked.
fn sign_in(home: &Path, label: &str) {
    fs::write(
        folder(home, label).join(".claude.json"),
        format!(r#"{{"oauthAccount":{{"accountUuid":"{label}-login","emailAddress":"{label}@example.com"}}}}"#),
    )
    .unwrap();
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs()
}

/// One account in the ovm-limits snapshot shape.
fn snapshot_account(id: &str, label: &str, five_hour: f64, seven_day: f64) -> Value {
    json!({
        "id": id,
        "label": label,
        "provider": "claude",
        "captured_at": now(),
        "windows": [
            {"id": "five_hour", "label": "5h", "used_percent": five_hour},
            {"id": "seven_day", "label": "7d", "used_percent": seven_day},
        ],
    })
}

fn write_limits(home: &Path, accounts: Vec<Value>) {
    let path = home.join(".ovm/limits/limits.json");
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, json!({ "accounts": accounts }).to_string()).unwrap();
}

struct Launch {
    stdout: String,
    stderr: String,
}

impl Launch {
    /// The value the fake Claude printed for `var`, `<unset>` when absent.
    fn var(&self, var: &str) -> &str {
        let prefix = format!("{var}=");
        self.stdout
            .lines()
            .find_map(|line| line.strip_prefix(&prefix))
            .unwrap_or_else(|| panic!("{var} not in fake claude output:\n{}", self.stdout))
    }

    fn args(&self) -> &str {
        self.var("args")
    }
}

fn launch(home: &Path, args: &[&str]) -> Launch {
    let output = ovm(home).arg("run").args(args).output().unwrap();
    let launched = Launch {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert!(
        output.status.success(),
        "ovm run failed\nstdout:\n{}\nstderr:\n{}",
        launched.stdout,
        launched.stderr
    );
    launched
}

/// Asserts the launch ran as the subscription account `label`.
fn assert_ran_as(home: &Path, launched: &Launch, label: &str) {
    assert_eq!(
        launched.var("CLAUDE_CONFIG_DIR"),
        folder(home, label).to_str().unwrap(),
        "{}",
        launched.stderr
    );
    assert_eq!(launched.var(ACCOUNT_ENV), label, "{}", launched.stderr);
}

/// Two subscription accounts `a` and `b`, chained a → b at the default
/// threshold.
fn chained_a_b() -> tempfile::TempDir {
    let home = home();
    accounts(home.path(), &["add", "a"]);
    accounts(home.path(), &["add", "b"]);
    accounts(home.path(), &["failover", "a", "b"]);
    sign_in(home.path(), "a");
    sign_in(home.path(), "b");
    home
}

#[test]
fn a_spent_first_account_fails_over_to_the_next_subscription() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 4.0, 96.0),
            snapshot_account("claude-2", "b", 2.0, 10.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert_eq!(launched.var("CLAUDE_CODE_USE_AZURE"), "<unset>");
    assert_eq!(launched.var("CLAUDE_CODE_USE_BEDROCK"), "<unset>");
    assert!(
        launched
            .stderr
            .contains("b (b@example.com, personal) — failover chain (a 7d 96% ≥ 95%, skipped; b 7d 10% < 95%)"),
        "{}",
        launched.stderr
    );
}

#[test]
fn skipped_accounts_are_narrated_before_the_launch_line() {
    let home = home();
    for label in ["a", "b", "c"] {
        accounts(home.path(), &["add", label]);
        sign_in(home.path(), label);
    }
    accounts(home.path(), &["failover", "a", "b", "c"]);
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 96.0, 10.0),
            snapshot_account("claude-2", "b", 20.0, 99.0),
            snapshot_account("claude-3", "c", 2.0, 30.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "c");
    let lines: Vec<&str> = launched.stderr.lines().map(str::trim).collect();
    let skip = lines
        .iter()
        .position(|line| *line == "↷ skipped a (5h 96% ≥ 95%), b (7d 99% ≥ 95%)")
        .unwrap_or_else(|| panic!("no skip line:\n{}", launched.stderr));
    let pick = lines
        .iter()
        .position(|line| line.starts_with("→ c ("))
        .unwrap_or_else(|| panic!("no launch line:\n{}", launched.stderr));
    assert_eq!(skip + 1, pick, "{}", launched.stderr);
    assert!(
        lines[pick].ends_with(
            "— failover chain (a 5h 96% ≥ 95%, skipped; b 7d 99% ≥ 95%, skipped; c 7d 30% < 95%)"
        ),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_first_pick_has_no_skip_line() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 40.0, 12.0)],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(!launched.stderr.contains("skipped"), "{}", launched.stderr);
    assert!(
        launched
            .stderr
            .contains("a (a@example.com, personal) — failover chain (a 5h 40% < 95%)"),
        "{}",
        launched.stderr
    );
}

#[test]
fn an_unsigned_account_still_says_why_the_chain_picked_it() {
    let home = home();
    accounts(home.path(), &["add", "a"]);
    accounts(home.path(), &["add", "b"]);
    accounts(home.path(), &["failover", "a", "b"]);
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 10.0, 96.0)],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains(
            "→ b (not signed in) — failover chain (a 7d 96% ≥ 95%, skipped; no usage data, first available)"
        ),
        "{}",
        launched.stderr
    );
    assert!(
        launched
            .stderr
            .contains("! b is not signed in yet — type /login"),
        "{}",
        launched.stderr
    );
}

#[test]
fn five_hour_peak_counts_against_the_threshold() {
    // Decision: the highest window of any length decides, so a 5h burst
    // skips an account whose week is still mostly free.
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 97.0, 10.0),
            snapshot_account("claude-2", "b", 2.0, 10.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains("↷ skipped a (5h 97% ≥ 95%)"),
        "{}",
        launched.stderr
    );
    assert!(
        launched
            .stderr
            .contains("failover chain (a 5h 97% ≥ 95%, skipped; b 7d 10% < 95%)"),
        "{}",
        launched.stderr
    );
}

#[test]
fn all_accounts_spent_launches_the_first_anyway() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 10.0, 96.0),
            snapshot_account("claude-2", "b", 99.0, 10.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(
        launched
            .stderr
            .contains("failover chain (all accounts above 95% threshold, using first)"),
        "{}",
        launched.stderr
    );
}

#[test]
fn without_limits_data_the_first_entry_wins() {
    let home = chained_a_b();
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(
        launched
            .stderr
            .contains("failover chain (no usage data, first available)"),
        "{}",
        launched.stderr
    );
}

#[test]
fn an_account_missing_from_the_snapshot_counts_as_available() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 10.0, 96.0)],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains("no usage data"),
        "{}",
        launched.stderr
    );
}

#[test]
fn an_account_under_another_label_in_the_snapshot_counts_as_available() {
    // Gap: limits.json is matched by label (else id) against accounts.json
    // labels. When ovm-limits knows `b` by another name, its 99% is never
    // seen and `b` silently looks free.
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 10.0, 96.0),
            snapshot_account("claude-2", "b-under-another-name", 99.0, 99.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains("no usage data"),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_spent_subscription_fails_over_to_azure() {
    let home = home();
    accounts(home.path(), &["add", "a"]);
    accounts(
        home.path(),
        &[
            "add",
            "azure-eu",
            "--azure",
            "https://eu.example.cognitiveservices.azure.com",
            "--api-key",
            "test-key",
        ],
    );
    accounts(home.path(), &["failover", "a", "azure-eu"]);
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 10.0, 96.0)],
    );
    let launched = launch(home.path(), &[]);
    assert_eq!(launched.var("CLAUDE_CODE_USE_AZURE"), "1");
    assert_eq!(
        launched.var("ANTHROPIC_BASE_URL"),
        "https://eu.example.cognitiveservices.azure.com"
    );
    assert_eq!(launched.var("ANTHROPIC_API_KEY"), "test-key");
    assert_eq!(launched.var("CLAUDE_CODE_USE_BEDROCK"), "<unset>");
    // A provider account still gets its own folder as the config dir (shared
    // brain, separate policy), never a subscription account's folder.
    assert_eq!(
        launched.var("CLAUDE_CONFIG_DIR"),
        folder(home.path(), "azure-eu").to_str().unwrap()
    );
    assert_eq!(launched.var(ACCOUNT_ENV), "azure-eu");
    assert!(
        launched
            .stderr
            .contains("azure-eu (Azure) — failover chain (a 7d 96% ≥ 95%, skipped; API account, no usage limits)"),
        "{}",
        launched.stderr
    );
    assert!(
        launched
            .stderr
            .contains("$ this account uses Azure API billing"),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_spent_subscription_fails_over_to_bedrock() {
    let home = home();
    accounts(home.path(), &["add", "a"]);
    accounts(
        home.path(),
        &[
            "add",
            "bedrock-us",
            "--bedrock",
            "us-east-1",
            "--profile",
            "work-sso",
        ],
    );
    accounts(home.path(), &["failover", "a", "bedrock-us"]);
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 10.0, 96.0)],
    );
    let launched = launch(home.path(), &[]);
    assert_eq!(launched.var("CLAUDE_CODE_USE_BEDROCK"), "1");
    assert_eq!(launched.var("AWS_REGION"), "us-east-1");
    assert_eq!(launched.var("AWS_PROFILE"), "work-sso");
    assert_eq!(launched.var("CLAUDE_CODE_USE_AZURE"), "<unset>");
    assert_eq!(launched.var("ANTHROPIC_BASE_URL"), "<unset>");
    assert_eq!(
        launched.var("CLAUDE_CONFIG_DIR"),
        folder(home.path(), "bedrock-us").to_str().unwrap()
    );
    assert_eq!(launched.var(ACCOUNT_ENV), "bedrock-us");
    assert!(
        launched
            .stderr
            .contains("bedrock-us (Bedrock) — failover chain (a 7d 96% ≥ 95%, skipped; API account, no usage limits)"),
        "{}",
        launched.stderr
    );
    assert!(
        launched
            .stderr
            .contains("$ this account uses Bedrock API billing"),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_binding_beats_the_chain_even_when_spent() {
    let home = chained_a_b();
    let repo = home.path().join("work/repo");
    fs::create_dir_all(&repo).unwrap();
    accounts(home.path(), &["bind", repo.to_str().unwrap(), "a"]);
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 100.0, 100.0),
            snapshot_account("claude-2", "b", 0.0, 0.0),
        ],
    );
    let output = ovm(home.path())
        .current_dir(&repo)
        .arg("run")
        .output()
        .unwrap();
    assert!(output.status.success());
    let launched = Launch {
        stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
    };
    assert_ran_as(home.path(), &launched, "a");
    assert!(launched.stderr.contains("bound to"), "{}", launched.stderr);
}

#[test]
fn a_named_account_beats_the_chain_even_when_spent() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 100.0, 100.0),
            snapshot_account("claude-2", "b", 0.0, 0.0),
        ],
    );
    let launched = launch(home.path(), &["a", "--resume", "abc123"]);
    assert_ran_as(home.path(), &launched, "a");
    assert_eq!(launched.args(), "--resume abc123");
    assert!(
        launched.stderr.contains("named on the command line"),
        "{}",
        launched.stderr
    );
}

/// `snapshot_account` taken `minutes` ago.
fn aged(mut account: Value, minutes: u64) -> Value {
    account["captured_at"] = json!(now() - minutes * SECONDS_PER_MINUTE);
    account
}

#[test]
fn a_stale_snapshot_is_treated_as_unknown() {
    // Older than twice the poll interval (60 min when config.json is silent):
    // `a`'s 96% may long since have reset, so the chain starts at the top.
    let home = chained_a_b();
    let three_days = 3 * MINUTES_PER_DAY;
    write_limits(
        home.path(),
        vec![aged(
            snapshot_account("claude-1", "a", 10.0, 96.0),
            three_days,
        )],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(
        launched.stderr.contains(&format!(
            "failover chain (usage data {three_days} min old, treating as unknown; first available)"
        )),
        "{}",
        launched.stderr
    );
    assert!(!launched.stderr.contains("skipped"), "{}", launched.stderr);
}

#[test]
fn the_stale_rule_honours_interval_minutes_from_config() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![aged(snapshot_account("claude-1", "a", 10.0, 96.0), 45)],
    );
    // No config.json: ovm-limits' default interval (60 min) applies, so a
    // 45 min old snapshot is fresh, the 96% counts and `a` is skipped.
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains("↷ skipped a (7d 96% ≥ 95%)"),
        "{}",
        launched.stderr
    );
    // A 15 min interval: 45 min is stale and `a` is tried first.
    fs::write(
        home.path().join(".ovm/limits/config.json"),
        r#"{"accounts": [], "interval_minutes": 15}"#,
    )
    .unwrap();
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(
        launched.stderr.contains(
            "failover chain (usage data 45 min old, treating as unknown; first available)"
        ),
        "{}",
        launched.stderr
    );
}

#[test]
fn arguments_after_selection_reach_claude_unchanged() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![snapshot_account("claude-1", "a", 10.0, 96.0)],
    );
    let launched = launch(home.path(), &["--version", "-p", "hello world"]);
    assert_ran_as(home.path(), &launched, "b");
    assert_eq!(launched.args(), "--version -p hello world");
}

/// Rewrites accounts.json so the chain names labels the CLI refuses to add
/// (`ovm accounts failover` checks every label; `add` only makes Claude
/// accounts, so a Codex account can only get here by hand).
fn set_chain_by_hand(home: &Path, chain: &[&str]) {
    let path = home.join(".ovm/accounts.json");
    let mut doc: Value = serde_json::from_str(&fs::read_to_string(&path).unwrap()).unwrap();
    doc["failover"]["chain"] = json!(chain);
    fs::write(&path, serde_json::to_string_pretty(&doc).unwrap()).unwrap();
}

#[test]
fn a_chain_label_that_is_not_an_account_is_skipped() {
    let home = chained_a_b();
    set_chain_by_hand(home.path(), &["codex-1", "a", "b"]);
    write_limits(
        home.path(),
        vec![
            snapshot_account("codex-1", "codex-1", 0.0, 0.0),
            snapshot_account("claude-1", "a", 10.0, 96.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
}

#[test]
fn an_unknown_first_entry_does_not_break_the_all_spent_fallback() {
    // The all-spent fallback takes the first chain entry that is an account.
    let home = chained_a_b();
    set_chain_by_hand(home.path(), &["codex-1", "a", "b"]);
    write_limits(
        home.path(),
        vec![
            snapshot_account("claude-1", "a", 99.0, 99.0),
            snapshot_account("claude-2", "b", 99.0, 99.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "a");
    assert!(
        launched
            .stderr
            .contains("failover chain (all accounts above 95% threshold, using first)"),
        "{}",
        launched.stderr
    );
    assert!(
        !launched.stderr.contains("not an account"),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_chain_of_non_accounts_falls_through_to_the_default() {
    let home = chained_a_b();
    accounts(home.path(), &["default", "b"]);
    set_chain_by_hand(home.path(), &["codex-1", "codex-2"]);
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched.stderr.contains("the default account"),
        "{}",
        launched.stderr
    );
}

#[test]
fn a_malformed_snapshot_entry_does_not_blank_the_rest() {
    let home = chained_a_b();
    write_limits(
        home.path(),
        vec![
            json!({"provider": "claude", "windows": []}),
            json!({"id": "claude-9", "label": "b"}),
            snapshot_account("claude-1", "a", 10.0, 96.0),
            snapshot_account("claude-2", "b", 2.0, 10.0),
        ],
    );
    let launched = launch(home.path(), &[]);
    assert_ran_as(home.path(), &launched, "b");
    assert!(
        launched
            .stderr
            .contains("failover chain (a 7d 96% ≥ 95%, skipped; b 7d 10% < 95%)"),
        "{}",
        launched.stderr
    );
}
