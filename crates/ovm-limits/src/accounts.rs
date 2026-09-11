//! Everything that changes an account, in one place, so the command line and
//! the registry screen cannot drift: register, sign in, rename, remove, poll,
//! and tear the whole thing down.

use crate::paths::{display, LimitsDirs};
use crate::registry::{Account, Provider, Registry};
use crate::{agent, claude, codex, due, events, hooks, snapshot, LimitsError, Result};
use console::style;
use std::path::PathBuf;
use std::time::Duration;

/// Sixty, not thirty: the app-server answers in a second when the machine is
/// idle and missed a 30s window twice in one afternoon while it compiled.
const CODEX_TIMEOUT: Duration = Duration::from_secs(60);

/// The registry, or the reason there is nothing to act on.
pub fn require(dirs: &LimitsDirs) -> Result<Registry> {
    Registry::load(&dirs.config_file())?
        .filter(|registry| !registry.accounts.is_empty())
        .ok_or_else(|| {
            LimitsError::Message(
                "no accounts yet — run `ovm limits` to add one (nothing is polled or written \
                 until then)"
                    .into(),
            )
        })
}

/// What `add` was asked for beyond the product.
#[derive(Debug, Default, Clone)]
pub struct AddOptions {
    pub label: Option<String>,
    pub home: Option<PathBuf>,
    pub model: Option<String>,
}

/// Register an account. Its id is assigned here; its home is created at
/// sign-in, so a registration nobody signs in to costs a line in a file.
pub fn add(dirs: &LimitsDirs, provider: Provider, options: AddOptions) -> Result<Account> {
    if provider == Provider::Codex && options.model.is_some() {
        return Err(LimitsError::Message(
            "--model applies to claude accounts only".into(),
        ));
    }
    if let Some(home) = &options.home {
        refuse_default_home(provider, home)?;
    }
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    let mut account = Account::new(provider, registry.next_id(provider), options.label);
    account.home = options.home;
    account.model = options.model;
    let account = registry.push(account)?.clone();
    registry.save(&path)?;
    dirs.ensure_layout()?;
    Ok(account)
}

/// Run the product's own sign-in inside the account's home. The browser flow
/// is Anthropic's or OpenAI's; this only sets the home and waits. Signing in
/// again replaces whatever login the home held.
pub fn login(dirs: &LimitsDirs, account: &Account) -> Result<()> {
    let home = account.poll_home(dirs);
    refuse_default_home(account.provider, &home)?;
    std::fs::create_dir_all(&home)?;
    let mut command = match account.provider {
        Provider::Claude => {
            let mut command = claude::product_command(account, &home);
            command.args(["auth", "login"]);
            command
        }
        Provider::Codex => {
            let mut command = codex::product_command(account, &home);
            command.arg("login");
            command
        }
    };
    eprintln!(
        "  {} signing in {} (home: {})",
        style("→").dim(),
        account.display(),
        display(&home)
    );
    let status = command.status()?;
    if !status.success() {
        return Err(LimitsError::Message(format!(
            "{} login exited with {status}",
            account.provider
        )));
    }
    Ok(())
}

/// Signing in to the home the person works in is the one thing this tool
/// must never do: the new grant would displace the one their own sessions
/// hold.
fn refuse_default_home(provider: Provider, home: &std::path::Path) -> Result<()> {
    match provider {
        Provider::Claude => claude::refuse_default_home(home),
        Provider::Codex => codex::refuse_default_home(home),
    }
}

pub fn rename(dirs: &LimitsDirs, key: &str, label: Option<String>) -> Result<Account> {
    let path = dirs.config_file();
    let mut registry = require(dirs)?;
    let account = registry.relabel(key, label)?;
    registry.save(&path)?;
    // Nothing on disk moves: the id still names the home, so the login that
    // home holds is untouched. The merged view carries the label, though.
    relabel_snapshot(dirs, &account)?;
    snapshot::merge(dirs)?;
    Ok(account)
}

fn relabel_snapshot(dirs: &LimitsDirs, account: &Account) -> Result<()> {
    let path = dirs.snapshot_file(&account.id);
    let Ok(raw) = std::fs::read_to_string(&path) else {
        return Ok(());
    };
    let Ok(mut stored) = serde_json::from_str::<snapshot::AccountSnapshot>(&raw) else {
        return Ok(());
    };
    stored.label.clone_from(&account.label);
    snapshot::write_snapshot(dirs, &stored)
}

/// What removing an account did with its home.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HomeFate {
    /// The home this tool made for the account is gone, login and all.
    Deleted(PathBuf),
    /// An explicit `--home` belongs to the person and is left in place; only
    /// the trust entry this tool added is taken back out.
    Kept(PathBuf),
}

/// Forget an account and everything this tool made for it. Clean-up runs
/// while the account is still registered, so a failure leaves it listed and
/// a retry can find its files.
pub fn remove(dirs: &LimitsDirs, key: &str) -> Result<(Account, HomeFate)> {
    let path = dirs.config_file();
    let mut registry = require(dirs)?;
    let account = registry.find(key).cloned().ok_or_else(|| {
        LimitsError::Message(format!("no account `{key}` — see: ovm limits list"))
    })?;
    let home = account.poll_home(dirs);
    let fate = if account.owns_home() {
        if home.exists() {
            std::fs::remove_dir_all(&home)?;
        }
        HomeFate::Deleted(home)
    } else {
        if account.provider == Provider::Claude {
            claude::unseed_claude_json(&claude::claude_json_path(&home), &dirs.scratch_dir())?;
        }
        HomeFate::Kept(home)
    };
    for stale in [
        dirs.snapshot_file(&account.id),
        dirs.capture_file(&account.id),
    ] {
        if stale.is_file() {
            std::fs::remove_file(stale)?;
        }
    }
    registry.remove(&account.id)?;
    registry.save(&path)?;
    // Always rebuild, so limits.json never names an account the registry no
    // longer has.
    snapshot::merge(dirs)?;
    Ok((account, fate))
}

/// Which accounts a poll covers.
#[derive(Debug, Default, Clone)]
pub struct Selection {
    pub provider: Option<Provider>,
    pub account: Option<String>,
    /// Only what the interval or a reset makes worth polling — the shape the
    /// background agent runs every tick — and stay quiet unless something
    /// happened.
    pub due_only: bool,
}

/// How a poll went: what was written, what failed, what changed.
#[derive(Debug, Default, Clone, PartialEq)]
pub struct Polled {
    pub polled: usize,
    pub failures: usize,
    pub events: Vec<events::Event>,
}

/// Poll the selected accounts, one after another, narrating to stderr.
pub fn poll(dirs: &LimitsDirs, registry: &Registry, selection: &Selection) -> Result<Polled> {
    let accounts: Vec<&Account> = registry
        .accounts
        .iter()
        .filter(|a| selection.provider.is_none_or(|p| a.provider == p))
        .filter(|a| {
            selection
                .account
                .as_ref()
                .is_none_or(|key| a.answers_to(key))
        })
        .collect();
    if accounts.is_empty() {
        return Err(LimitsError::Message(
            "no accounts match — see: ovm limits list".into(),
        ));
    }
    dirs.ensure_layout()?;
    let previous = snapshot::load_snapshots(dirs)?;
    let now = snapshot::now();
    let mut outcome = Polled::default();
    for account in accounts {
        let last = previous
            .iter()
            .find(|s| s.provider == account.provider && s.id == account.id);
        if selection.due_only && !due::is_due(account, last, registry.interval_minutes, now) {
            continue;
        }
        let quiet = selection.due_only && account.provider == Provider::Codex;
        if !quiet {
            eprint!("  {} {} … ", style("→").dim(), account.display());
        }
        let result = match account.provider {
            Provider::Claude => claude::poll(account, dirs, claude::Timeouts::default()),
            Provider::Codex => codex::poll(account, dirs, CODEX_TIMEOUT),
        };
        let mut snapshot = match result {
            Ok(snapshot) => {
                if !quiet {
                    eprintln!("{}", style(summary(&snapshot)).green());
                }
                snapshot
            }
            Err(error) => {
                outcome.failures += 1;
                if quiet {
                    eprint!("  {} {} … ", style("→").dim(), account.display());
                }
                eprintln!("{} {error}", style("✗").red());
                let source = match account.provider {
                    Provider::Claude => claude::SOURCE,
                    Provider::Codex => codex::SOURCE,
                };
                snapshot::AccountSnapshot::failed(account, source, error.to_string())
            }
        };
        // What changed since last time, and the memory of it: a window that
        // reset keeps the moment, every other window keeps what it had.
        let noticed = events::detect(last, &mut snapshot, account.signed_in(dirs));
        for window in &mut snapshot.windows {
            window.last_reset_at = noticed
                .iter()
                .filter(|e| {
                    matches!(e.kind, events::Kind::Reset | events::Kind::SurpriseReset)
                        && e.window_id.as_deref() == Some(window.id.as_str())
                })
                .map(|e| e.at)
                .next()
                .or_else(|| {
                    last.and_then(|l| l.windows.iter().find(|w| w.id == window.id))
                        .and_then(|w| w.last_reset_at)
                });
        }
        for event in &noticed {
            eprintln!("  {} {}", style("!").yellow().bold(), event.text);
        }
        outcome.polled += 1;
        snapshot::write_snapshot(dirs, &snapshot)?;
        events::append(dirs, &noticed)?;
        outcome.events.extend(noticed);
    }
    if outcome.polled == 0 {
        return Ok(outcome);
    }
    let merged = snapshot::merge(dirs)?;
    if !selection.due_only || outcome.failures > 0 {
        eprintln!(
            "  {} {} account(s) → {}",
            style("✓").green(),
            merged.accounts.len(),
            display(&dirs.merged_file())
        );
    }
    // Hooks run last, once everything they might want to read is on disk.
    hooks::on_events(dirs, registry, &outcome.events);
    hooks::on_poll(dirs, registry, outcome.polled, outcome.events.len());
    Ok(outcome)
}

/// Change how often a Claude account is polled at most.
pub fn set_interval(dirs: &LimitsDirs, minutes: u64) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    let previous = registry.interval_minutes;
    registry.interval_minutes = minutes;
    if let Err(error) = registry.validate() {
        registry.interval_minutes = previous;
        return Err(error);
    }
    registry.save(&path)?;
    Ok(registry)
}

/// Set or clear a hook.
pub fn set_hook(dirs: &LimitsDirs, name: &str, command: Option<String>) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    registry.hooks.set(name, command).ok_or_else(|| {
        LimitsError::Message(format!(
            "no hook called `{name}` — hooks are on-event and on-poll"
        ))
    })?;
    registry.save(&path)?;
    Ok(registry)
}

/// `5h 19% (resets in 4h 10m), 7d 26% (resets in 2d 10h)` — one line per
/// poll, for the narration.
pub fn summary(snapshot: &snapshot::AccountSnapshot) -> String {
    if snapshot.windows.is_empty() {
        return "no windows reported".into();
    }
    let now = snapshot::now();
    snapshot
        .windows
        .iter()
        .map(|w| crate::show::window_brief(w, now))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Remove the agent, every home this tool made, the trust entries it added
/// to homes it did not, and the whole limits directory.
pub fn purge(dirs: &LimitsDirs) -> Result<Vec<String>> {
    let mut done = Vec::new();
    if agent::uninstall()? {
        done.push("agent removed".into());
    }
    if let Some(registry) = Registry::load(&dirs.config_file())? {
        for account in registry
            .accounts
            .iter()
            .filter(|a| a.provider == Provider::Claude && !a.owns_home())
        {
            let path = claude::claude_json_path(&account.poll_home(dirs));
            if claude::unseed_claude_json(&path, &dirs.scratch_dir())? {
                done.push(format!("trust entry removed from {}", display(&path)));
            }
        }
    }
    if dirs.base().exists() {
        std::fs::remove_dir_all(dirs.base())?;
        done.push(format!("removed {}", display(dirs.base())));
    }
    Ok(done)
}
