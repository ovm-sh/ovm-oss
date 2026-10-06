//! limits — how much of each Claude Code / Codex subscription is used, and
//! when each window resets.
//!
//! An OVM plugin (`ovm limits …`). Polls go through the products themselves:
//! Claude via a throwaway interactive session whose statusline hands over
//! `rate_limits`, Codex via `codex app-server`'s `account/rateLimits/read`.
//! No credential is read, copied, or sent anywhere by this binary. Results
//! land in `~/.ovm/limits/limits.json` for anything that wants to show them.
//!
//! `ovm limits` on a terminal opens the account registry: every registered
//! login, its latest numbers, and the keys to add, sign in, rename, remove,
//! and poll. The subcommands below do the same things for scripts.
//!
//! The binary ships with every OVM bundle but is inert until an account is
//! registered — the registry file is the activation switch.

mod accounts;
mod agent;
mod announcements;
mod claude;
mod codex;
mod doctor;
mod due;
mod events;
mod hooks;
mod live;
mod paths;
mod plan;
mod pty;
mod public;
mod registry;
mod serve;
mod show;
mod snapshot;
mod table;
mod tui;

use accounts::{AddOptions, HomeFate, Selection};
use console::style;
use paths::LimitsDirs;
use registry::Provider;
use std::io::IsTerminal;
use std::path::PathBuf;

pub(crate) type Result<T> = std::result::Result<T, LimitsError>;

/// Tells `ovm <product>` to launch what is installed instead of auto-updating
/// first. A string contract with the launcher (`ovm`'s `autoupdate` module
/// names the same variable); this crate does not depend on it.
///
/// Every poll sets it. Polls run on a clock — 45s to Claude's statusline, 60s
/// to Codex's app-server — and an inline download of a new release takes
/// minutes, so a release landing used to blind the poller until someone ran
/// the product by hand.
pub(crate) const NO_AUTO_UPDATE_ENV: &str = "OVM_NO_AUTO_UPDATE";

/// `ovm <product>`, the way every poll launches one: through OVM, so the poll
/// runs the version OVM has selected, and with [`NO_AUTO_UPDATE_ENV`] set so
/// it runs that version *now* rather than downloading a newer one first.
pub(crate) fn ovm_launcher(product: &str) -> std::process::Command {
    let mut command = std::process::Command::new("ovm");
    command.arg(product);
    command.env(NO_AUTO_UPDATE_ENV, "1");
    command
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum LimitsError {
    #[error("{0}")]
    Message(String),
    #[error(transparent)]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let result = match args.first().map(String::as_str) {
        None | Some("setup") => registry_screen(),
        Some("list") => list(),
        Some("add") => add(&args[1..]),
        Some("login") => login(&args[1..]),
        Some("rename") => rename(&args[1..]),
        Some("pause") => pause(&args[1..], true),
        Some("resume") => pause(&args[1..], false),
        Some("remove") => remove(&args[1..]),
        Some("poll") => poll(&args[1..]),
        Some("show") => show(&args[1..]),
        Some("serve") => serve(&args[1..]),
        Some("events") => events_command(&args[1..]),
        Some("hook") => hook_command(&args[1..]),
        Some("interval") => interval(&args[1..]),
        Some("digest") => digest(&args[1..]),
        Some("sort") => sort(&args[1..]),
        Some("group") => group(&args[1..]),
        Some("plan") => plan_command(&args[1..]),
        Some("agent") => agent_command(&args[1..]),
        Some("doctor") => LimitsDirs::new().and_then(|dirs| doctor::run(&dirs)),
        Some("uninstall") => uninstall(&args[1..]),
        Some("path") => LimitsDirs::new().map(|dirs| println!("{}", dirs.merged_file().display())),
        Some("help") | Some("--help") | Some("-h") => {
            print_help();
            Ok(())
        }
        Some("--version") | Some("-V") => {
            println!("limits {}", env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Some(other) => Err(LimitsError::Message(format!(
            "unknown command `{other}` — see: ovm limits help"
        ))),
    };
    if let Err(error) = result {
        eprintln!("  {} {error}", style("✗").red().bold());
        std::process::exit(1);
    }
}

/// `ovm limits` — the registry on a terminal, the help anywhere else.
fn registry_screen() -> Result<()> {
    if !on_a_terminal() {
        print_help();
        return Ok(());
    }
    let dirs = LimitsDirs::new()?;
    tui::run(&dirs)
}

fn on_a_terminal() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// `list` — every account, its home, and whether it has signed in.
fn list() -> Result<()> {
    let dirs = LimitsDirs::new()?;
    let registry = registry::Registry::load_or_default(&dirs.config_file())?;
    if registry.accounts.is_empty() {
        println!("  no accounts yet — run: ovm limits");
        return Ok(());
    }
    for account in &registry.accounts {
        let state = if account.paused {
            "  (paused)"
        } else if account.signed_in(&dirs) {
            ""
        } else {
            "  (not signed in)"
        };
        println!(
            "  {:<22} {}{state}",
            account.display(),
            paths::display(&account.poll_home(&dirs))
        );
    }
    Ok(())
}

/// `add [claude|codex] [label] [--home DIR] [--model M] [--no-login]
///  [--live-only] [--team|--personal]`
///
/// On a terminal, whatever is not said is asked: which product, what label.
/// In a script nothing is asked and nothing opens a browser.
fn add(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    let mut provider = args.first().and_then(|a| Provider::parse(a));
    let mut options = AddOptions::default();
    let mut login_after = true;
    let positional_start = usize::from(provider.is_some());
    let mut iter = args[positional_start..].iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--home" => {
                let value = iter
                    .next()
                    .ok_or_else(|| usage("--home needs a directory"))?;
                options.home = Some(absolute(PathBuf::from(value)));
            }
            "--model" => {
                options.model = Some(
                    iter.next()
                        .ok_or_else(|| usage("--model needs a name"))?
                        .clone(),
                );
            }
            "--no-login" => login_after = false,
            "--live-only" => options.live_only = true,
            "--team" => options.kind = Some("team".into()),
            "--personal" => options.kind = Some("personal".into()),
            other if other.starts_with("--") => {
                return Err(usage(&format!("unknown add flag `{other}`")))
            }
            other if options.label.is_none() => options.label = Some(other.to_string()),
            other => return Err(usage(&format!("unexpected argument `{other}`"))),
        }
    }
    let interactive = on_a_terminal();
    if provider.is_none() {
        if !interactive {
            return Err(usage("add <claude|codex> [label]"));
        }
        provider = tui::ask_provider()?;
        if options.label.is_none() {
            options.label = tui::ask_label(None)?;
        }
    }
    let Some(provider) = provider else {
        return Ok(());
    };
    let account = accounts::add(&dirs, provider, options)?;
    println!(
        "  {} added {}  home: {}",
        style("✓").green(),
        account.display(),
        paths::display(&account.poll_home(&dirs))
    );
    if account.live_only {
        println!(
            "    live-only: never polled; its numbers arrive with the next statusline of a session in that home"
        );
        return Ok(());
    }
    if login_after && interactive && tui::ask_yes_no("Sign in now?")? {
        println!();
        accounts::login(&dirs, &account)?;
        println!(
            "  {} {} signed in — next: ovm limits poll --account {}",
            style("✓").green(),
            account.display(),
            account.id
        );
        return Ok(());
    }
    println!("  sign in once: ovm limits login {}", account.id);
    Ok(())
}

/// `login <account>` — sign an account in, or replace the login it has.
fn login(args: &[String]) -> Result<()> {
    let [key] = args else {
        return Err(usage(
            "login <account>   (an id like claude-1, or its label)",
        ));
    };
    let dirs = LimitsDirs::new()?;
    let registry = accounts::require(&dirs)?;
    let account = registry.find(key).ok_or_else(|| {
        LimitsError::Message(format!("no account `{key}` — see: ovm limits list"))
    })?;
    accounts::login(&dirs, account)?;
    println!(
        "  {} {} signed in — next: ovm limits poll --account {}",
        style("✓").green(),
        account.display(),
        account.id
    );
    Ok(())
}

/// `rename <account> [label]` — no label clears it.
fn rename(args: &[String]) -> Result<()> {
    let (key, label) = match args {
        [key] => (key, None),
        [key, label] => (key, Some(label.clone())),
        _ => return Err(usage("rename <account> [new label]   (no label clears it)")),
    };
    let dirs = LimitsDirs::new()?;
    let account = accounts::rename(&dirs, key, label)?;
    println!("  {} now called {}", style("✓").green(), account.display());
    Ok(())
}

/// `pause <account>` / `resume <account>` — set aside without forgetting.
fn pause(args: &[String], paused: bool) -> Result<()> {
    let verb = if paused { "pause" } else { "resume" };
    let [key] = args else {
        return Err(usage(&format!("{verb} <account>")));
    };
    let dirs = LimitsDirs::new()?;
    let account = accounts::pause(&dirs, key, paused)?;
    if paused {
        println!(
            "  {} {} paused — no polls, out of limits.json; home and login kept. Back with: ovm limits resume {key}",
            style("✓").green(),
            account.display()
        );
    } else {
        println!(
            "  {} {} resumed — the next poll fills it back in",
            style("✓").green(),
            account.display()
        );
    }
    Ok(())
}

/// `remove <account> [--yes]`
fn remove(args: &[String]) -> Result<()> {
    let (key, yes) = match args {
        [key] => (key, false),
        [key, flag] if flag == "--yes" || flag == "-y" => (key, true),
        _ => return Err(usage("remove <account> [--yes]")),
    };
    let dirs = LimitsDirs::new()?;
    let registry = accounts::require(&dirs)?;
    let account = registry.find(key).ok_or_else(|| {
        LimitsError::Message(format!("no account `{key}` — see: ovm limits list"))
    })?;
    if !yes {
        if !on_a_terminal() {
            return Err(LimitsError::Message(
                "refusing to remove without --yes when not on a terminal".into(),
            ));
        }
        let what = if account.owns_home() {
            "and its sign-in"
        } else {
            "(its home stays)"
        };
        if !tui::ask_yes_no(&format!("Remove {} {what}?", account.display()))? {
            println!("  Kept.");
            return Ok(());
        }
    }
    let (account, fate) = accounts::remove(&dirs, &account.id)?;
    println!("  {} removed {}", style("✓").green(), account.display());
    match fate {
        HomeFate::Deleted(home) => println!("    home deleted: {}", paths::display(&home)),
        HomeFate::Kept(home) => println!("    home left in place: {}", paths::display(&home)),
    }
    Ok(())
}

/// `poll [--due] [--only claude|codex] [--account NAME]`
fn poll(args: &[String]) -> Result<()> {
    let mut selection = Selection::default();
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--due" => selection.due_only = true,
            "--only" => {
                let value = iter
                    .next()
                    .ok_or_else(|| usage("--only needs claude|codex"))?;
                selection.provider =
                    Some(Provider::parse(value).ok_or_else(|| usage("--only needs claude|codex"))?);
            }
            "--account" => {
                selection.account = Some(
                    iter.next()
                        .ok_or_else(|| usage("--account needs a name"))?
                        .clone(),
                );
            }
            other => return Err(usage(&format!("unknown poll flag `{other}`"))),
        }
    }
    let dirs = LimitsDirs::new()?;
    let registry = accounts::require(&dirs)?;
    let polled = accounts::poll(&dirs, &registry, &selection)?;
    if polled.failures > 0 {
        return Err(LimitsError::Message(format!(
            "{} account(s) failed to poll — snapshots record the error",
            polled.failures
        )));
    }
    Ok(())
}

/// `show [--json | --brief | --grid | <account>]`
fn show(args: &[String]) -> Result<()> {
    let (overrides, rest) = show::Overrides::take(args)?;
    let format = match rest.as_slice() {
        [] => show::Format::Table,
        [flag] if flag == "--json" => show::Format::Json,
        [flag] if flag == "--brief" => show::Format::Brief,
        [flag] if flag == "--grid" => show::Format::Grid,
        [account] if !account.starts_with('-') => {
            let dirs = LimitsDirs::new()?;
            accounts::require(&dirs)?;
            return show::run_account(&dirs, account);
        }
        _ => {
            return Err(usage(
                "show takes --json, --brief, --grid, or one account's id or label; the table also takes --sort provider|reset and --group / --no-group",
            ))
        }
    };
    let dirs = LimitsDirs::new()?;
    accounts::require(&dirs)?;
    show::run(&dirs, format, overrides)
}

/// `agent install | uninstall | status`
fn agent_command(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    match args.first().map(String::as_str) {
        None | Some("status") => {
            println!("  {}", agent::describe());
            Ok(())
        }
        Some("install") => {
            let mut every = None;
            let mut iter = args[1..].iter();
            while let Some(arg) = iter.next() {
                match arg.as_str() {
                    "--every" => {
                        let value = iter
                            .next()
                            .ok_or_else(|| usage("--every needs an interval like 15m"))?;
                        every = Some(registry::parse_interval(value)?);
                    }
                    other => {
                        return Err(usage(&format!(
                            "agent install [--every 15m] (unknown `{other}`)"
                        )))
                    }
                }
            }
            let registry = match every {
                Some(minutes) => accounts::set_interval(&dirs, minutes)?,
                None => accounts::require(&dirs)?,
            };
            if registry.accounts.is_empty() {
                accounts::require(&dirs)?;
            }
            let path = agent::install(&dirs)?;
            println!(
                "  {} agent installed: {} — every {} min, a Claude turn at most every {}",
                style("✓").green(),
                paths::display(&path),
                agent::TICK_SECONDS / 60,
                registry::interval_label(registry.interval_minutes)
            );
            Ok(())
        }
        Some("uninstall") => {
            if agent::uninstall()? {
                println!("  {} agent removed", style("✓").green());
            } else {
                println!("  no agent installed");
            }
            Ok(())
        }
        Some(other) => Err(usage(&format!(
            "agent install|uninstall|status (unknown `{other}`)"
        ))),
    }
}

/// `uninstall [--yes]` — the agent, every home this tool made, and the
/// whole limits directory.
fn uninstall(args: &[String]) -> Result<()> {
    let yes = args.iter().any(|a| a == "--yes" || a == "-y");
    let dirs = LimitsDirs::new()?;
    if !dirs.base().exists() && !agent::is_installed() {
        println!("  ovm limits has nothing installed — nothing to remove.");
        return Ok(());
    }
    if !yes {
        if !on_a_terminal() {
            return Err(LimitsError::Message(
                "refusing to uninstall without --yes when not on a terminal".into(),
            ));
        }
        if !tui::ask_yes_no(&format!(
            "Remove the agent, every account's sign-in, and {}?",
            paths::display(dirs.base())
        ))? {
            println!("  Kept.");
            return Ok(());
        }
    }
    for line in accounts::purge(&dirs)? {
        println!("  {} {line}", style("✓").green());
    }
    Ok(())
}

/// `events [--since 24h] [--json]` — what the polls noticed.
fn events_command(args: &[String]) -> Result<()> {
    let mut since_minutes: Option<u64> = None;
    let mut as_json = false;
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--json" => as_json = true,
            "--since" => {
                let value = iter
                    .next()
                    .ok_or_else(|| usage("--since needs a span like 24h"))?;
                since_minutes = Some(registry::parse_interval(value)?);
            }
            other => {
                return Err(usage(&format!(
                    "events [--since 24h] [--json] (unknown `{other}`)"
                )))
            }
        }
    }
    let dirs = LimitsDirs::new()?;
    let now = snapshot::now();
    let since = since_minutes.map_or(0, |m| now.saturating_sub(m * 60));
    let found = events::load(&dirs, since)?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&found)?);
        return Ok(());
    }
    if found.is_empty() {
        println!(
            "  no events{}",
            if since > 0 { " in that span" } else { " yet" }
        );
        return Ok(());
    }
    for event in &found {
        println!("  {}", events::line(event, now));
    }
    Ok(())
}

/// `hook list | on-event <cmd> | on-poll <cmd> | on-digest <cmd> | clear <name> | test [name] | ntfy <topic> | publish`
fn hook_command(args: &[String]) -> Result<()> {
    const USAGE: &str =
        "hook list | on-event <command> | on-poll <command> | on-digest <command> | clear <name> | test [name] | ntfy <topic> | publish";
    let dirs = LimitsDirs::new()?;
    match args.first().map(String::as_str) {
        None | Some("list") => {
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            for name in ["on-event", "on-poll", "on-digest"] {
                match registry.hooks.get(name).flatten() {
                    Some(command) => println!("  {name:<10} {command}"),
                    None => println!("  {name:<10} {}", style("(none)").dim()),
                }
            }
            println!();
            println!("  on-event runs once per event with OVM_LIMITS_EVENT (JSON, also on stdin),");
            println!(
                "  OVM_LIMITS_EVENT_TEXT, OVM_LIMITS_EVENT_KIND (surprise_reset, credit_reset, limit_lifted, reset,"
            );
            println!(
                "  threshold, retrying, failed, recovered), OVM_LIMITS_ACCOUNT, OVM_LIMITS_LABEL,"
            );
            println!("  OVM_LIMITS_WINDOW, OVM_LIMITS_LEVEL (the threshold crossed).");
            println!(
                "  on-poll runs once after a poll with OVM_LIMITS_POLLED and OVM_LIMITS_EVENTS."
            );
            println!("  on-digest runs right after on-poll with the same environment.");
            println!("  All see OVM_LIMITS_FILE, OVM_LIMITS_PUBLIC_FILE, OVM_LIMITS_EVENTS_FILE.");
            Ok(())
        }
        Some(name @ ("on-event" | "on-poll" | "on-digest")) => {
            let command = args.get(1).cloned().ok_or_else(|| usage(USAGE))?;
            accounts::set_hook(&dirs, name, Some(command.clone()))?;
            println!("  {} {name} → {command}", style("✓").green());
            Ok(())
        }
        Some("clear") => {
            let name = args.get(1).ok_or_else(|| usage(USAGE))?;
            accounts::set_hook(&dirs, name, None)?;
            println!("  {} {name} cleared", style("✓").green());
            Ok(())
        }
        Some("test") => {
            let name = args.get(1).map(String::as_str).unwrap_or("on-event");
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            let Some(command) = registry.hooks.get(name).flatten() else {
                return Err(LimitsError::Message(format!(
                    "no {name} hook is set — see: ovm limits hook list"
                )));
            };
            let ok = if name == "on-poll" || name == "on-digest" {
                // Only the hook under test, not whatever else shares the poll.
                let mut probe = registry.clone();
                probe.hooks = hooks::Hooks::default();
                probe.hooks.set(name, Some(command.to_string()));
                hooks::on_poll(&dirs, &probe, 0, 0);
                true
            } else {
                let sample = events::sample(snapshot::now());
                println!("  {} sending: {}", style("→").dim(), sample.text);
                let mut probe = registry.clone();
                probe.hooks.on_event = Some(command.to_string());
                hooks::on_events(&dirs, &probe, &[sample]);
                true
            };
            if ok {
                println!(
                    "  {} {name} ran (any failure is printed above)",
                    style("✓").green()
                );
            }
            Ok(())
        }
        Some("ntfy") => {
            let topic = args.get(1).ok_or_else(|| usage("hook ntfy <topic>"))?;
            // What reaches a phone is usage, not the poller's own weather. A
            // single missed poll is a busy machine and says nothing; its
            // recovery says less. Both used to be sent and the thresholds
            // were not, which is exactly backwards — the run that found an
            // account at 100% stayed silent while four flapping polls buzzed
            // (2026-09-08). So: the bars, a reset nobody asked for, a credit
            // the operator spent, and a poll that has failed twice.
            let command = format!(
                r#"src="[$(hostname -s)]"; case "$OVM_LIMITS_EVENT_KIND" in surprise_reset) title="limits: early reset"; tags=rotating_light;; threshold) case "$OVM_LIMITS_LEVEL" in 100) title="limits: hit"; tags=octagonal_sign;; *) title="limits: $OVM_LIMITS_LEVEL%"; tags=chart_with_upwards_trend;; esac;; limit_lifted) title="limits: back"; tags=white_check_mark;; credit_reset) title="limits: credit spent"; tags=information_source;; failed) title="limits: poll failing"; tags=x;; recovered) title="limits: poll recovered"; tags=white_check_mark;; *) exit 0;; esac; curl -fsS -H "Title: $src $title" -H "Tags: $tags" -d "$OVM_LIMITS_EVENT_TEXT" "https://ntfy.sh/{topic}" >/dev/null"#
            );
            accounts::set_hook(&dirs, "on-event", Some(command.clone()))?;
            println!(
                "  {} on-event → ntfy topic {topic} (limits: hit / back, thresholds, an early reset, a spent credit, a poll failing twice and recovering; titled [hostname] so the phone shows which machine sent it)",
                style("✓").green()
            );
            // The digest: every window of every account on one line, after
            // every poll, at ntfy's low priority so it shows in the tray and
            // never buzzes. An account that is failing says so instead of
            // its numbers. Asked for on 2026-09-19: "my limits update hourly
            // unless an anomaly is detected".
            let digest = format!(
                r#"msg=$(python3 -c 'import json, sys
doc = json.load(open(sys.argv[1]))
parts = []
for account in doc.get("accounts", []):
    if account.get("error"):
        parts.append(account["label"] + " failing")
        continue
    windows = " ".join(w["label"] + " " + str(int(round(w["used_percent"]))) + "%" for w in account.get("windows", []))
    parts.append(account["label"] + " " + windows)
print(" | ".join(parts))' "$OVM_LIMITS_FILE") && curl -fsS -H "Title: [$(hostname -s)] limits: hourly" -H "Priority: low" -H "Tags: bar_chart" -d "$msg" "https://ntfy.sh/{topic}" >/dev/null"#
            );
            accounts::set_hook(&dirs, "on-digest", Some(digest))?;
            println!(
                "  {} on-digest → one low-priority line per poll with every account's windows",
                style("✓").green()
            );
            println!("    subscribe in the ntfy app to `{topic}`; test with: ovm limits hook test");
            Ok(())
        }
        Some("publish") => {
            let token = dirs.base().join("blob-token");
            let command = format!(
                r#"curl -fsS -X PUT "https://blob.vercel-storage.com/resets.json" -H "authorization: Bearer $(cat '{}')" -H "x-api-version: 7" -H "x-content-type: application/json" -H "x-add-random-suffix: 0" -H "x-allow-overwrite: 1" -H "x-cache-control-max-age: 60" --data-binary @"$OVM_LIMITS_PUBLIC_FILE" >/dev/null"#,
                token.display()
            );
            accounts::set_hook(&dirs, "on-poll", Some(command))?;
            println!(
                "  {} on-poll → upload resets.json to Vercel Blob",
                style("✓").green()
            );
            if !token.is_file() {
                println!(
                    "    {} put the store's read-write token in {} (mode 600) before the next poll",
                    style("!").yellow(),
                    paths::display(&token)
                );
            }
            Ok(())
        }
        Some(_) => Err(usage(USAGE)),
    }
}

/// `digest [1h]` — how often the digest hook may run at most.
fn digest(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    match args {
        [] => {
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            println!("  {}", digest_label(registry.digest_minutes));
            Ok(())
        }
        [value] => {
            let minutes = registry::parse_interval(value)?;
            let registry = accounts::set_digest_minutes(&dirs, minutes)?;
            println!(
                "  {} {}",
                style("✓").green(),
                digest_label(registry.digest_minutes)
            );
            Ok(())
        }
        _ => Err(usage("digest [1h|30m|0]")),
    }
}

fn digest_label(minutes: u64) -> String {
    if minutes == 0 {
        "the digest hook runs on every poll".to_string()
    } else {
        format!(
            "the digest hook runs at most every {}",
            registry::interval_label(minutes)
        )
    }
}

/// `sort [provider|reset]` — the order of the account table's rows.
fn sort(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    match args {
        [] => {
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            println!("  {}", sort_label(registry.table_sort));
            Ok(())
        }
        [value] => {
            let sort = table::SortOrder::parse(value)
                .ok_or_else(|| usage(&format!("sort provider|reset (not `{value}`)")))?;
            let registry = accounts::set_table_sort(&dirs, sort)?;
            println!(
                "  {} {}",
                style("✓").green(),
                sort_label(registry.table_sort)
            );
            Ok(())
        }
        _ => Err(usage("sort [provider|reset]")),
    }
}

fn sort_label(sort: table::SortOrder) -> String {
    match sort {
        table::SortOrder::Provider => {
            "the account table sorts by provider, then soonest reset".to_string()
        }
        table::SortOrder::Reset => {
            "the account table sorts by soonest reset, across providers".to_string()
        }
    }
}

/// `group [on|off]` — the account table as one block per provider.
fn group(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    match args {
        [] => {
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            println!("  {}", group_label(registry.table_grouped));
            Ok(())
        }
        [value] => {
            let grouped = match value.as_str() {
                "on" => true,
                "off" => false,
                other => return Err(usage(&format!("group on|off (not `{other}`)"))),
            };
            let registry = accounts::set_table_grouped(&dirs, grouped)?;
            println!(
                "  {} {}",
                style("✓").green(),
                group_label(registry.table_grouped)
            );
            Ok(())
        }
        _ => Err(usage("group [on|off]")),
    }
}

fn group_label(grouped: bool) -> String {
    if grouped {
        "the account table draws one block per provider".to_string()
    } else {
        "the account table is one list".to_string()
    }
}

/// `plan <account> [--name …] [--status live|cancelled] [--ends YYYY-MM-DD]
/// [--clear]` — the subscription as the billing page states it. No flags
/// prints the record.
fn plan_command(args: &[String]) -> Result<()> {
    let Some((key, flags)) = args.split_first() else {
        return Err(usage(plan::PLAN_USAGE));
    };
    if key.starts_with("--") {
        return Err(usage(plan::PLAN_USAGE));
    }
    let edit = plan::PlanEdit::parse(flags)?;
    let dirs = LimitsDirs::new()?;
    let today = table::local_day(snapshot::now());
    if edit == plan::PlanEdit::Show {
        let registry = registry::Registry::load_or_default(&dirs.config_file())?;
        let account = registry.find(key).ok_or_else(|| {
            LimitsError::Message(format!("no account `{key}` — see: ovm limits list"))
        })?;
        match &account.plan {
            Some(plan) => println!("  {}: {}", account.display(), show::plan_line(plan, today)),
            None => println!(
                "  {}: no plan recorded — set one: ovm limits plan {key} --status cancelled --ends YYYY-MM-DD",
                account.display()
            ),
        }
        return Ok(());
    }
    let registry = registry::Registry::load_or_default(&dirs.config_file())?;
    let current = registry.find(key).and_then(|account| account.plan.clone());
    let account = accounts::set_plan(&dirs, key, edit.apply(current))?;
    match &account.plan {
        Some(plan) => {
            println!(
                "  {} {}: {}",
                style("✓").green(),
                account.display(),
                show::plan_line(plan, today)
            );
            if plan.status == plan::PlanStatus::Cancelled && plan.ends_on.is_none() {
                println!(
                    "  {}",
                    style(format!(
                        "add the day access ends, so the table can warn you: ovm limits plan {key} --ends YYYY-MM-DD"
                    ))
                    .dim()
                );
            }
        }
        None => println!(
            "  {} {}: plan forgotten",
            style("✓").green(),
            account.display()
        ),
    }
    Ok(())
}

/// `interval [15m]` — how often an account is polled at most.
fn interval(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    match args {
        [] => {
            let registry = registry::Registry::load_or_default(&dirs.config_file())?;
            println!(
                "  every account polled at most every {} (the agent ticks every {} min)",
                registry::interval_label(registry.interval_minutes),
                agent::TICK_SECONDS / 60
            );
            Ok(())
        }
        [value] => {
            let registry = accounts::set_interval(&dirs, registry::parse_interval(value)?)?;
            println!(
                "  {} every account polled at most every {}",
                style("✓").green(),
                registry::interval_label(registry.interval_minutes)
            );
            Ok(())
        }
        _ => Err(usage("interval [15m|1h|2h30m]")),
    }
}

fn absolute(path: PathBuf) -> PathBuf {
    if path.is_absolute() {
        return path;
    }
    if let Some(rest) = path.to_str().and_then(|p| p.strip_prefix("~/")) {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(&path))
        .unwrap_or(path)
}

/// `serve [--bind tailnet|local|ADDR:PORT] [--key]` — the read-only hub.
/// `--key` prints the bearer key (made on first use) and exits.
fn serve(args: &[String]) -> Result<()> {
    let dirs = LimitsDirs::new()?;
    let mut bind = serve::Bind::Tailnet(serve::DEFAULT_PORT);
    let mut iter = args.iter();
    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--bind" => {
                let value = iter
                    .next()
                    .ok_or_else(|| usage("serve --bind tailnet|local|ADDR:PORT"))?;
                bind = serve::Bind::parse(value)?;
            }
            "--key" => {
                println!("{}", serve::load_or_create_key(&dirs)?);
                return Ok(());
            }
            other => return Err(usage(&format!("unknown serve flag `{other}`"))),
        }
    }
    serve::run(&dirs, &bind)
}

fn usage(text: &str) -> LimitsError {
    LimitsError::Message(format!("usage: {text}"))
}

fn print_help() {
    println!(
        "limits {} — subscription usage windows for Claude Code and Codex",
        env!("CARGO_PKG_VERSION")
    );
    println!();
    println!("Usage: ovm limits            open the account registry (add, sign in, rename, remove, poll)");
    println!("       ovm limits <command>");
    println!();
    println!("Commands:");
    for (command, blurb) in [
        (
            "list",
            "Every account, its home, and whether it has signed in",
        ),
        (
            "add",
            "Register an account: add [claude|codex] [label] [--home DIR] [--model M] [--no-login] [--live-only] [--team|--personal]",
        ),
        (
            "login",
            "Sign an account in (or replace its login), inside its own home: login <account>",
        ),
        (
            "rename",
            "Change what you call an account: rename <account> [label]",
        ),
        (
            "pause",
            "Set an account aside without forgetting it (no polls, out of limits.json): pause <account>",
        ),
        (
            "resume",
            "Put a paused account back into the rotation: resume <account>",
        ),
        (
            "remove",
            "Forget an account and delete the home this tool made for it: remove <account> [--yes]",
        ),
        (
            "poll",
            "Ask each account for its windows (--due: only when worth it; --only, --account)",
        ),
        (
            "show",
            "Print the account table (--sort provider|reset, --group/--no-group for this call); show <account>: one account in full, --grid: every account side by side, --json: the structured endpoint, --brief: plain lines",
        ),
        (
            "serve",
            "limits.json as a read-only CLIProxyAPI-style hub for t3code: serve [--bind tailnet|local|ADDR:PORT] [--key]",
        ),
        (
            "interval",
            "How often an account may be polled at most: interval [15m|30m|1h]",
        ),
        (
            "digest",
            "How often the on-digest hook may run at most: digest [1h|30m|0 for every poll]",
        ),
        (
            "sort",
            "The order of the account table: sort [provider|reset]",
        ),
        (
            "group",
            "The account table as one block per provider: group [on|off]",
        ),
        (
            "plan",
            "Record an account's plan by hand: plan <account> [--name \"…\"] [--status live|cancelled] [--ends YYYY-MM-DD] [--clear]",
        ),
        (
            "agent",
            "Background poller: install | uninstall | status (macOS launchd)",
        ),
        (
            "doctor",
            "Check homes, logins, and the last poll of every account",
        ),
        (
            "uninstall",
            "Remove the agent, every sign-in, and ~/.ovm/limits (--yes)",
        ),
        (
            "path",
            "Print where limits.json lives, for anything that reads it directly",
        ),
    ] {
        println!("  {command:<10} {blurb}");
    }
    println!();
    println!("An account is `claude-1`, `codex-1`, … plus a label of your choosing. Each signs in");
    println!(
        "once, into a home only ovm limits uses — never ~/.claude or ~/.codex, because a poll"
    );
    println!("refreshes the login it runs as, and refreshing yours signs your own sessions out.");
    println!();
    println!("How a Claude poll works: a throwaway interactive Claude Code session in an empty");
    println!("directory, one word typed on the cheapest model, the statusline payload that");
    println!("follows carries the 5h / 7d windows, then /exit. Each poll is one real turn.");
    println!("Codex is asked over `codex app-server` and costs nothing — but it is polled on");
    println!("the same interval, because every poll runs the on-poll hooks.");
    println!();
    println!("The structured endpoint is `ovm limits show --json`: the merged limits.json, schema");
    println!(
        "`ovm-limits/v1`, one entry per account with its windows (used %, reset time, length),"
    );
    println!("plan, credits, reset credits, the model and version a poll ran on, and under `raw`");
    println!("everything the product said, verbatim. Fields are only ever added.");
    println!();
    println!("Nothing is polled, written, or spent until an account is registered.");
    println!("Env: OVM_LIMITS_HOME (default ~/.ovm/limits)");
}
