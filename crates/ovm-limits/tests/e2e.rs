//! End to end against fake products: a `claude` that behaves like the TUI
//! from the poller's point of view (runs the statusline at start, again with
//! `rate_limits` after a line is typed, exits on /exit) and a `codex` that
//! speaks just enough app-server JSON-RPC. Nothing here touches the network
//! or a real home.

use assert_cmd::Command;
use serde_json::Value;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

const FAKE_CLAUDE: &str = r#"#!/bin/sh
# Find the capture command inside the --settings JSON and run it like Claude
# Code's statusline would (sh -c, payload on stdin). python3 is on every CI
# runner; a sed one-liner mis-parsed the JSON escapes on the first try.
settings=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--settings" ]; then settings="$arg"; fi
  prev="$arg"
done
cmd=$(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["statusLine"]["command"])' "$settings")
[ -n "$cmd" ] || { echo "no statusline command in --settings" >&2; exit 3; }
[ -t 0 ] || { echo "not a tty" >&2; exit 4; }
printf '%s' '{"model":{"id":"claude-haiku"},"cwd":"'"$PWD"'"}' | sh -c "$cmd"
read -r line
printf '%s' '{"model":{"id":"claude-haiku"},"cost":{"total_cost_usd":0.0123},"rate_limits":{"five_hour":{"used_percentage":19,"resets_at":1788618000},"seven_day":{"used_percentage":26,"resets_at":1788771600}}}' | sh -c "$cmd"
read -r line
case "$line" in /exit*) exit 0;; *) exit 5;; esac
"#;

/// A fake Claude whose 5h usage and reset time come from `$FAKE_USAGE`
/// (`used resets_at`), so a later poll can report whatever the test wants.
const SCRIPTED_CLAUDE: &str = r#"#!/bin/sh
settings=""
prev=""
for arg in "$@"; do
  if [ "$prev" = "--settings" ]; then settings="$arg"; fi
  prev="$arg"
done
cmd=$(python3 -c 'import json, sys; print(json.loads(sys.argv[1])["statusLine"]["command"])' "$settings")
read -r used resets < "$FAKE_USAGE"
printf '%s' '{"model":{"id":"claude-haiku"}}' | sh -c "$cmd"
read -r line
printf '%s' '{"model":{"id":"claude-haiku"},"rate_limits":{"five_hour":{"used_percentage":'"$used"',"resets_at":'"$resets"'},"seven_day":{"used_percentage":26,"resets_at":1788771600}}}' | sh -c "$cmd"
read -r line
exit 0
"#;

const FAKE_CODEX: &str = r#"#!/bin/sh
[ "$1" = "app-server" ] || { echo "expected app-server" >&2; exit 3; }
[ -n "$CODEX_HOME" ] || { echo "CODEX_HOME not set" >&2; exit 4; }
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{"userAgent":"fake"}}';;
    *'"id":2'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"result":{"rateLimits":{"limitId":"codex","limitName":null,"primary":{"usedPercent":19,"windowDurationMins":10080,"resetsAt":1789192025},"secondary":null,"credits":{"hasCredits":false,"unlimited":false,"balance":"0"},"spendControlReached":false,"planType":"pro","rateLimitReachedType":null},"rateLimitsByLimitId":{"codex":{"limitId":"codex","limitName":null,"primary":{"usedPercent":19,"windowDurationMins":10080,"resetsAt":1789192025},"secondary":null,"planType":"pro"}},"rateLimitResetCredits":{"availableCount":1,"credits":[{"id":"RateLimitResetCredit_1","resetType":"codexRateLimits","status":"available","grantedAt":1788581906,"expiresAt":1791173906,"title":"Full reset","description":"one free reset"}]},"accountId":"acct-1"}}'; exit 0;;
  esac
done
"#;

const SIGNED_OUT_CODEX: &str = r#"#!/bin/sh
while IFS= read -r line; do
  case "$line" in
    *'"id":1'*) printf '%s\n' '{"jsonrpc":"2.0","id":1,"result":{}}';;
    *'"id":2'*) printf '%s\n' '{"jsonrpc":"2.0","id":2,"error":{"code":-32000,"message":"not logged in"}}'; exit 0;;
  esac
done
"#;

/// A fake `codex login`: writes the auth file the real one would, so the
/// account reads as signed in afterwards.
const FAKE_CODEX_LOGIN: &str = r#"#!/bin/sh
[ "$1" = "login" ] || { echo "expected login" >&2; exit 3; }
[ -n "$CODEX_HOME" ] || { echo "CODEX_HOME not set" >&2; exit 4; }
printf '{}' > "$CODEX_HOME/auth.json"
"#;

struct Fixture {
    _temp: tempfile::TempDir,
    home: PathBuf,
    claude_home: PathBuf,
    codex_home: PathBuf,
    fake_claude: PathBuf,
    fake_codex: PathBuf,
}

fn fixture(codex_script: &str) -> Fixture {
    let temp = tempfile::tempdir().unwrap();
    let home = temp.path().join("limits");
    let claude_home = temp.path().join("claude-home");
    let codex_home = temp.path().join("codex-home");
    fs::create_dir_all(&claude_home).unwrap();
    fs::create_dir_all(&codex_home).unwrap();
    fs::write(codex_home.join("auth.json"), "{}").unwrap();
    let fake_claude = script(temp.path(), "claude", FAKE_CLAUDE);
    let fake_codex = script(temp.path(), "codex", codex_script);
    Fixture {
        _temp: temp,
        home,
        claude_home,
        codex_home,
        fake_claude,
        fake_codex,
    }
}

fn script(dir: &Path, name: &str, body: &str) -> PathBuf {
    let path = dir.join(name);
    fs::write(&path, body).unwrap();
    fs::set_permissions(&path, fs::Permissions::from_mode(0o755)).unwrap();
    path
}

fn limits(fixture: &Fixture) -> Command {
    let mut command = Command::cargo_bin("ovm-limits").unwrap();
    command
        // A private HOME, so `~/Library/LaunchAgents` resolves inside the
        // sandbox and the default-home refusal has a home to compare against.
        .env("HOME", fixture._temp.path())
        .env("OVM_LIMITS_HOME", &fixture.home)
        .env(
            "OVM_LIMITS_LAUNCH_AGENTS_DIR",
            fixture._temp.path().join("agents"),
        )
        .env("OVM_LIMITS_NO_LAUNCHCTL", "1")
        .env("OVM_LIMITS_CLAUDE_CMD", &fixture.fake_claude)
        .env("OVM_LIMITS_CODEX_CMD", &fixture.fake_codex);
    command
}

/// Two explicit accounts, each polling a fixture home the fakes run in.
fn configure(fixture: &Fixture) {
    limits(fixture)
        .args(["add", "claude", "work", "--no-login", "--home"])
        .arg(&fixture.claude_home)
        .assert()
        .success()
        .stdout(predicates::str::contains("added claude-1 (work)"));
    limits(fixture)
        .args(["add", "codex", "spare", "--no-login", "--home"])
        .arg(&fixture.codex_home)
        .assert()
        .success()
        .stdout(predicates::str::contains("added codex-1 (spare)"));
}

fn merged(fixture: &Fixture) -> Value {
    serde_json::from_str(&fs::read_to_string(fixture.home.join("limits.json")).unwrap()).unwrap()
}

fn registry(fixture: &Fixture) -> Value {
    serde_json::from_str(&fs::read_to_string(fixture.home.join("config.json")).unwrap()).unwrap()
}

fn captured_at(merged: &Value, provider: &str) -> u64 {
    merged["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["provider"] == provider)
        .unwrap()["captured_at"]
        .as_u64()
        .unwrap()
}

#[test]
fn with_no_accounts_nothing_polls_writes_or_touches_a_home() {
    let fixture = fixture(FAKE_CODEX);
    for args in [
        vec!["poll"],
        vec!["show"],
        vec!["login", "claude-1"],
        vec!["rename", "claude-1", "x"],
        vec!["remove", "claude-1", "--yes"],
        vec!["agent", "install"],
    ] {
        limits(&fixture)
            .args(&args)
            .assert()
            .failure()
            .stderr(predicates::str::contains("no accounts yet"));
    }
    assert!(
        !fixture.home.exists(),
        "limits dir must not exist before an add"
    );
    assert!(
        !fixture.claude_home.join(".claude.json").exists(),
        "no trust entry before a poll"
    );
    // Read-only commands still answer, and a script gets the help instead of
    // a screen.
    limits(&fixture)
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("no accounts yet"));
    limits(&fixture)
        .assert()
        .success()
        .stdout(predicates::str::contains("open the account registry"));
    limits(&fixture)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicates::str::contains("no accounts yet"));
    limits(&fixture)
        .args(["uninstall", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("nothing to remove"));
}

#[test]
fn add_assigns_ids_per_product_and_a_script_must_name_the_product() {
    let fixture = fixture(FAKE_CODEX);
    limits(&fixture)
        .arg("add")
        .assert()
        .failure()
        .stderr(predicates::str::contains("usage: add <claude|codex>"));
    limits(&fixture)
        .args(["add", "claude", "--no-login"])
        .assert()
        .success()
        .stdout(predicates::str::contains("added claude-1 "));
    limits(&fixture)
        .args(["add", "claude", "second", "--no-login"])
        .assert()
        .success()
        .stdout(predicates::str::contains("added claude-2 (second)"));
    limits(&fixture)
        .args(["add", "codex", "--no-login"])
        .assert()
        .success()
        .stdout(predicates::str::contains("added codex-1 "));
    // A label is refused when it is taken or looks like an id; nothing is
    // registered for a refused add.
    limits(&fixture)
        .args(["add", "codex", "Second", "--no-login"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("two accounts are labelled"));
    limits(&fixture)
        .args(["add", "codex", "claude-1", "--no-login"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("is an account id"));
    assert_eq!(registry(&fixture)["accounts"].as_array().unwrap().len(), 3);
    // The product's own home is never an account's home.
    limits(&fixture)
        .args(["add", "codex", "--no-login", "--home"])
        .arg(fixture._temp.path().join(".codex"))
        .assert()
        .failure()
        .stderr(predicates::str::contains("refusing"));
    limits(&fixture)
        .arg("list")
        .assert()
        .success()
        .stdout(predicates::str::contains("claude-2 (second)"))
        .stdout(predicates::str::contains("(not signed in)"));
}

#[test]
fn rename_changes_the_label_everywhere_and_never_the_id() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    limits(&fixture).arg("poll").assert().success();
    limits(&fixture)
        .args(["rename", "work", "personal"])
        .assert()
        .success()
        .stdout(predicates::str::contains("now called claude-1 (personal)"));
    let accounts = merged(&fixture);
    let claude = accounts["accounts"]
        .as_array()
        .unwrap()
        .iter()
        .find(|a| a["provider"] == "claude")
        .unwrap();
    assert_eq!(
        claude["id"], "claude-1",
        "the id is what the home is named after"
    );
    assert_eq!(
        claude["label"], "personal",
        "the merged view follows the rename"
    );
    assert!(fixture.home.join("snapshots/claude-1.json").is_file());
    // Either name selects the account from here on; the old one does not.
    limits(&fixture)
        .args(["poll", "--account", "personal"])
        .assert()
        .success();
    limits(&fixture)
        .args(["poll", "--account", "work"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no accounts match"));
    limits(&fixture)
        .args(["rename", "claude-1"])
        .assert()
        .success()
        .stdout(predicates::str::contains("now called claude-1\n"));
}

#[test]
fn remove_deletes_the_home_this_tool_made_and_keeps_one_it_did_not() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    limits(&fixture).arg("poll").assert().success();
    // A third account in a home of its own, signed in by the fake login.
    let login = script(fixture._temp.path(), "codex-login", FAKE_CODEX_LOGIN);
    limits(&fixture)
        .args(["add", "codex", "own", "--no-login"])
        .assert()
        .success();
    limits(&fixture)
        .env("OVM_LIMITS_CODEX_CMD", &login)
        .args(["login", "own"])
        .assert()
        .success()
        .stdout(predicates::str::contains("codex-2 (own) signed in"));
    let own_home = fixture.home.join("homes/codex-2");
    assert!(
        own_home.join("auth.json").is_file(),
        "the login landed in the account's own home"
    );
    limits(&fixture)
        .args(["poll", "--account", "own"])
        .assert()
        .success();
    assert!(fixture.home.join("snapshots/codex-2.json").is_file());

    // A script must say --yes; then the home, snapshot, and registry entry go.
    limits(&fixture)
        .args(["remove", "own"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("--yes"));
    limits(&fixture)
        .args(["remove", "own", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("removed codex-2 (own)"))
        .stdout(predicates::str::contains("home deleted"));
    assert!(!own_home.exists(), "the home this tool made is gone");
    assert!(!fixture.home.join("snapshots/codex-2.json").exists());
    assert!(
        !merged(&fixture)["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .any(|a| a["id"] == "codex-2"),
        "limits.json never names an account the registry no longer has"
    );

    // An explicit home belongs to the person: it stays, minus our trust entry.
    let scratch = fixture.home.join("scratch");
    let claude_json = fixture.claude_home.join(".claude.json");
    limits(&fixture)
        .args(["remove", "work", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("home left in place"));
    assert!(fixture.claude_home.is_dir());
    let root: Value = serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert!(
        root["projects"]
            .get(scratch.to_string_lossy().as_ref())
            .is_none(),
        "the scratch trust entry is gone"
    );
    assert_eq!(
        root["hasCompletedOnboarding"], true,
        "the rest of the file survives"
    );
    // The freed id is handed out again only once nothing later exists.
    limits(&fixture)
        .args(["add", "claude", "--no-login"])
        .assert()
        .success()
        .stdout(predicates::str::contains("added claude-1 "));
}

#[test]
fn poll_due_spends_a_claude_turn_only_when_the_interval_or_a_reset_says_so() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    // The fake Claude reports five_hour.resets_at = 1788618000.
    let t0: u64 = 1_788_617_000;
    limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", t0.to_string())
        .args(["poll", "--due"])
        .assert()
        .success();
    let first = merged(&fixture);
    assert_eq!(captured_at(&first, "claude"), t0);
    assert_eq!(captured_at(&first, "codex"), t0);

    // Five minutes later: Codex again (free), Claude untouched, and quiet.
    let tick = limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", (t0 + 300).to_string())
        .args(["poll", "--due"])
        .assert()
        .success();
    assert!(
        tick.get_output().stderr.is_empty(),
        "a quiet tick logs nothing"
    );
    let second = merged(&fixture);
    assert_eq!(
        captured_at(&second, "claude"),
        t0,
        "inside the interval, no reset yet"
    );
    assert_eq!(captured_at(&second, "codex"), t0 + 300);

    // The 5h window reset at 1788618000 → the next tick after it polls Claude
    // even though the hour has not passed.
    let t_reset = 1_788_618_000 + 60;
    limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", t_reset.to_string())
        .args(["poll", "--due"])
        .assert()
        .success();
    assert_eq!(captured_at(&merged(&fixture), "claude"), t_reset);

    // Then the plain interval takes over again.
    limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", (t_reset + 1800).to_string())
        .args(["poll", "--due"])
        .assert()
        .success();
    assert_eq!(captured_at(&merged(&fixture), "claude"), t_reset);
    limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", (t_reset + 3600).to_string())
        .args(["poll", "--due"])
        .assert()
        .success();
    assert_eq!(captured_at(&merged(&fixture), "claude"), t_reset + 3600);
}

#[test]
fn agent_install_writes_the_plist_and_uninstall_removes_everything() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    limits(&fixture).arg("poll").assert().success();
    let scratch = fixture.home.join("scratch");
    let claude_json = fixture.claude_home.join(".claude.json");
    assert!(
        claude_json.is_file(),
        "the poll seeded trust in the isolated home"
    );

    limits(&fixture)
        .args(["agent", "install"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent installed"));
    let plist_path = fixture
        ._temp
        .path()
        .join("agents")
        .join("sh.ovm.limits.plist");
    let plist = fs::read_to_string(&plist_path).unwrap();
    assert!(
        plist.contains(
            "<string>limits</string>\n    <string>poll</string>\n    <string>--due</string>"
        ),
        "{plist}"
    );
    assert!(
        plist.contains(&format!("<string>{}</string>", fixture.home.display())),
        "agent is pointed at this limits home"
    );
    limits(&fixture)
        .args(["agent", "status"])
        .assert()
        .success()
        .stdout(predicates::str::contains("installed:"));

    limits(&fixture)
        .args(["uninstall", "--yes"])
        .assert()
        .success()
        .stdout(predicates::str::contains("agent removed"))
        .stdout(predicates::str::contains("trust entry removed"));
    assert!(!plist_path.exists());
    assert!(!fixture.home.exists());
    let root: Value = serde_json::from_str(&fs::read_to_string(&claude_json).unwrap()).unwrap();
    assert!(
        root["projects"]
            .get(scratch.to_string_lossy().as_ref())
            .is_none(),
        "the scratch trust entry is gone"
    );
    assert_eq!(
        root["hasCompletedOnboarding"], true,
        "the rest of the file survives"
    );
}

#[test]
fn poll_then_show_covers_both_providers() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);

    limits(&fixture).arg("poll").assert().success();

    let merged = merged(&fixture);
    assert_eq!(merged["schema"], "ovm-limits/v1");
    let accounts = merged["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 2);

    let claude = &accounts[0];
    assert_eq!(claude["provider"], "claude");
    assert_eq!(claude["id"], "claude-1");
    assert_eq!(claude["label"], "work");
    assert_eq!(claude["source"], "statusline");
    assert_eq!(claude["windows"][0]["id"], "five_hour");
    assert_eq!(claude["windows"][0]["used_percent"], 19.0);
    assert_eq!(claude["windows"][1]["resets_at"], 1788771600);
    assert_eq!(claude["poll_cost_usd"], 0.0123);
    assert_eq!(claude["model"], "claude-haiku");
    assert_eq!(
        claude["raw"]["five_hour"]["resets_at"], 1788618000,
        "the product's own object rides along"
    );
    assert!(claude.get("error").is_none());

    let codex = &accounts[1];
    assert_eq!(codex["provider"], "codex");
    assert_eq!(codex["source"], "app-server");
    assert_eq!(codex["plan"], "pro");
    assert_eq!(codex["windows"][0]["id"], "codex.primary");
    assert_eq!(codex["reset_credits_available"], 1);
    assert_eq!(codex["reset_credits"][0]["title"], "Full reset");
    assert_eq!(codex["reset_credits"][0]["expires_at"], 1791173906);
    assert_eq!(codex["account_id"], "acct-1");
    assert_eq!(codex["credits"]["has_credits"], false);
    assert_eq!(codex["spend_control_reached"], false);
    assert_eq!(codex["raw"]["rateLimits"]["planType"], "pro");

    // The scratch directory was pre-trusted in the isolated Claude home, and
    // that home's onboarding was seeded — the real ~/.claude.json is untouched.
    let claude_json: Value = serde_json::from_str(
        &fs::read_to_string(fixture.claude_home.join(".claude.json")).unwrap(),
    )
    .unwrap();
    assert_eq!(claude_json["hasCompletedOnboarding"], true);
    let scratch = fixture.home.join("scratch");
    assert_eq!(
        claude_json["projects"][scratch.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
        true
    );

    let table = limits(&fixture).arg("show").assert().success();
    let text = String::from_utf8_lossy(&table.get_output().stdout).into_owned();
    assert!(text.contains("claude-1 (work)"), "{text}");
    assert!(text.contains("5h"), "{text}");
    assert!(text.contains("codex-1 (spare) (pro)"), "{text}");
    assert!(
        text.contains("reset credit: Full reset (available"),
        "{text}"
    );
    assert!(text.contains("poll: on claude-haiku"), "{text}");

    let json = limits(&fixture).args(["show", "--json"]).assert().success();
    let shown: Value = serde_json::from_slice(&json.get_output().stdout).unwrap();
    assert_eq!(shown, merged);
}

#[test]
fn a_signed_out_account_is_recorded_as_a_failure_and_poll_exits_nonzero() {
    let fixture = fixture(SIGNED_OUT_CODEX);
    configure(&fixture);

    limits(&fixture)
        .args(["poll", "--only", "codex"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("not logged in"));

    let merged = merged(&fixture);
    let accounts = merged["accounts"].as_array().unwrap();
    assert_eq!(accounts.len(), 1, "only codex was polled");
    assert!(accounts[0]["error"]
        .as_str()
        .unwrap()
        .contains("not logged in"));
    assert!(accounts[0]["windows"].as_array().unwrap().is_empty());
}

#[test]
fn after_an_add_show_needs_a_poll_first_and_doctor_reports_never_polled() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    limits(&fixture)
        .arg("show")
        .assert()
        .failure()
        .stderr(predicates::str::contains("ovm limits poll"));
    limits(&fixture)
        .arg("doctor")
        .assert()
        .success()
        .stdout(predicates::str::contains("never polled"));
}

#[test]
fn a_registry_from_before_ids_is_refused_with_the_way_out() {
    let fixture = fixture(FAKE_CODEX);
    fs::create_dir_all(&fixture.home).unwrap();
    fs::write(
        fixture.home.join("config.json"),
        r#"{"accounts": [{"name": "default", "provider": "claude"}], "interval_minutes": 60}"#,
    )
    .unwrap();
    limits(&fixture)
        .arg("poll")
        .assert()
        .failure()
        .stderr(predicates::str::contains("predates the account registry"))
        .stderr(predicates::str::contains("ovm limits uninstall --yes"));
}

/// The headline: usage that fell while the window was not due is a surprise
/// reset — logged, in limits.json, on the public feed, and handed to the
/// hooks. A scheduled rollover is logged and kept off the feed.
#[test]
fn a_surprise_reset_is_noticed_published_and_handed_to_the_hooks() {
    let fixture = fixture(FAKE_CODEX);
    let claude = script(fixture._temp.path(), "claude-scripted", SCRIPTED_CLAUDE);
    let usage = fixture._temp.path().join("usage.txt");
    let seen = fixture._temp.path().join("seen.txt");
    limits(&fixture)
        .args(["add", "claude", "work", "--no-login", "--home"])
        .arg(&fixture.claude_home)
        .assert()
        .success();
    limits(&fixture)
        .args(["hook", "on-event"])
        .arg(format!(
            "echo \"$OVM_LIMITS_EVENT_KIND $OVM_LIMITS_WINDOW $OVM_LIMITS_EVENT_TEXT\" >> '{}'",
            seen.display()
        ))
        .assert()
        .success();
    limits(&fixture)
        .args(["hook", "on-poll"])
        .arg(format!(
            "echo \"poll $OVM_LIMITS_POLLED $OVM_LIMITS_EVENTS\" >> '{}'",
            seen.display()
        ))
        .assert()
        .success();
    limits(&fixture)
        .args(["hook", "list"])
        .assert()
        .success()
        .stdout(predicates::str::contains("on-event  echo"));

    // Poll 1: 64% used, window resets far ahead.
    let t0: u64 = 1_788_617_000;
    fs::write(&usage, "64 1788700000").unwrap();
    let poll = |now: u64| {
        limits(&fixture)
            .env("OVM_LIMITS_CLAUDE_CMD", &claude)
            .env("FAKE_USAGE", &usage)
            .env("OVM_LIMITS_FAKE_NOW", now.to_string())
            .args(["poll", "--only", "claude"])
            .assert()
            .success()
    };
    poll(t0);
    // Poll 2: usage climbed. Nothing to say.
    fs::write(&usage, "70 1788700000").unwrap();
    poll(t0 + 900);
    // Poll 3: usage fell to 2% with the reset still hours away — a surprise.
    fs::write(&usage, "2 1788700000").unwrap();
    poll(t0 + 1800);
    // Poll 4: the window was due and rolled over — the clock, not news.
    fs::write(&usage, "1 1788800000").unwrap();
    poll(1_788_700_100);

    let log = fs::read_to_string(&seen).unwrap();
    assert!(
        log.contains("threshold 5h Claude Code 5h window at 70%"),
        "{log}"
    );
    assert!(log.contains("surprise_reset 5h Claude Code 5h window reset early · claude-1 (work) · was 70%, now 2%"), "{log}");
    assert!(
        log.contains("reset 5h Claude Code 5h window rolled over"),
        "{log}"
    );
    assert!(log.contains("poll 1 0\n"), "{log}");
    assert!(log.contains("poll 1 1\n"), "{log}");

    let events = limits(&fixture)
        .args(["events", "--json"])
        .assert()
        .success();
    let events: Value = serde_json::from_slice(&events.get_output().stdout).unwrap();
    let kinds: Vec<&str> = events
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap())
        .collect();
    assert_eq!(kinds, ["threshold", "surprise_reset", "reset"]);
    assert_eq!(events[1]["at"], t0 + 1800);
    assert_eq!(events[1]["previous_used_percent"], 70.0);
    limits(&fixture)
        .args(["events", "--since", "1h"])
        .env("OVM_LIMITS_FAKE_NOW", (1_788_700_100 + 60).to_string())
        .assert()
        .success()
        .stdout(predicates::str::contains("rolled over"))
        .stdout(predicates::str::contains("1m ago"));

    let merged = merged(&fixture);
    assert_eq!(
        merged["accounts"][0]["windows"][0]["last_reset_at"],
        1_788_700_100
    );
    assert_eq!(merged["events"].as_array().unwrap().len(), 3);
    assert_eq!(merged["last_poll_at"], 1_788_700_100);
    assert!(merged.get("next_poll_at").is_none(), "no agent installed");

    let feed: Value =
        serde_json::from_str(&fs::read_to_string(fixture.home.join("resets.json")).unwrap())
            .unwrap();
    assert_eq!(feed["schema"], "ovm-limits/resets-v1");
    let resets = feed["resets"].as_array().unwrap();
    assert_eq!(resets.len(), 1, "only the surprise reset is public");
    assert_eq!(resets[0]["window_label"], "5h");
    assert_eq!(resets[0]["provider"], "claude");
    let text = fs::read_to_string(fixture.home.join("resets.json")).unwrap();
    for private in ["used_percent", "work", "claude-1", "\"host\""] {
        assert!(
            !text.contains(private),
            "{private} leaked into the public feed: {text}"
        );
    }
}

#[test]
fn the_interval_and_the_agent_schedule_are_one_setting() {
    let fixture = fixture(FAKE_CODEX);
    configure(&fixture);
    limits(&fixture)
        .arg("interval")
        .assert()
        .success()
        .stdout(predicates::str::contains("every 1h"));
    limits(&fixture)
        .args(["interval", "15m"])
        .assert()
        .success()
        .stdout(predicates::str::contains("every 15m"));
    limits(&fixture)
        .args(["interval", "2m"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("interval_minutes"));
    limits(&fixture)
        .args(["agent", "install", "--every", "30m"])
        .assert()
        .success()
        .stdout(predicates::str::contains("at most every 30m"));
    assert_eq!(registry(&fixture)["interval_minutes"], 30);
    // With the agent installed the merged view says when it next runs.
    limits(&fixture)
        .env("OVM_LIMITS_FAKE_NOW", "1788617000")
        .arg("poll")
        .assert()
        .success();
    let merged = merged(&fixture);
    assert_eq!(merged["interval_minutes"], 30);
    assert_eq!(merged["last_poll_at"], 1_788_617_000);
    // Codex is due at the next tick, five minutes on.
    assert_eq!(merged["next_poll_at"], 1_788_617_000 + 300);
    let feed: Value =
        serde_json::from_str(&fs::read_to_string(fixture.home.join("resets.json")).unwrap())
            .unwrap();
    assert_eq!(feed["next_poll_at"], 1_788_617_000 + 300);
    assert_eq!(feed["poll_every_minutes"], 30);
    // hook test fires a synthetic surprise reset through on-event.
    let seen = fixture._temp.path().join("seen.txt");
    limits(&fixture)
        .args(["hook", "ntfy", "topic-x"])
        .assert()
        .success();
    limits(&fixture)
        .args(["hook", "on-event"])
        .arg(format!("cat > '{}'", seen.display()))
        .assert()
        .success();
    limits(&fixture).args(["hook", "test"]).assert().success();
    let event: Value = serde_json::from_str(&fs::read_to_string(&seen).unwrap()).unwrap();
    assert_eq!(event["kind"], "surprise_reset");
    limits(&fixture)
        .args(["hook", "clear", "on-event"])
        .assert()
        .success();
    limits(&fixture)
        .args(["hook", "test"])
        .assert()
        .failure()
        .stderr(predicates::str::contains("no on-event hook"));
}

#[test]
fn version_and_help_answer_on_a_bare_machine() {
    let fixture = fixture(FAKE_CODEX);
    limits(&fixture)
        .arg("--version")
        .assert()
        .success()
        .stdout(predicates::str::starts_with("limits "));
    limits(&fixture)
        .arg("help")
        .assert()
        .success()
        .stdout(predicates::str::contains("ovm limits <command>"));
    limits(&fixture)
        .arg("bogus")
        .assert()
        .failure()
        .stderr(predicates::str::contains("unknown command"));
}
