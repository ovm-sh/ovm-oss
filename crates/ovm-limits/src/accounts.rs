//! Everything that changes an account, in one place, so the command line and
//! the registry screen cannot drift: register, sign in, rename, remove, poll,
//! and tear the whole thing down.

use crate::paths::{display, LimitsDirs};
use crate::plan::Plan;
use crate::registry::{Account, Provider, Registry};
use crate::{agent, claude, codex, due, events, hooks, live, snapshot, table, LimitsError, Result};
use console::{style, Term};
use std::path::PathBuf;
use std::time::Duration;

/// Sixty, not thirty: the app-server answers in a second when the machine is
/// idle and missed a 30s window twice in one afternoon while it compiled.
const CODEX_TIMEOUT: Duration = Duration::from_secs(60);

/// One app-server that never answers is not a broken account.
///
/// `codex app-server` sometimes wedges on startup refreshing its model
/// catalogue — `codex_models_manager: failed to refresh available models:
/// timeout waiting for child process to exit` — and then never reaches the
/// handshake, so the whole 60s budget burns. It is a fresh process every time
/// and a healthy one answers in about two seconds, so a second attempt costs
/// two seconds and clears it. 18 of the 24 poll failures in the fortnight to
/// 2026-09-20 were this, each one pushing a "failing" and a "recovered" to a
/// phone for a window whose numbers had not moved.
fn poll_codex_with_one_retry(
    account: &Account,
    dirs: &LimitsDirs,
) -> Result<snapshot::AccountSnapshot> {
    match codex::poll(account, dirs, CODEX_TIMEOUT) {
        Err(error) if is_startup_hang(&error) => codex::poll(account, dirs, CODEX_TIMEOUT),
        result => result,
    }
}

/// A timeout waiting for the app-server to speak — the retryable shape. A
/// refusal, a bad login or a missing binary is an answer, and repeating it
/// only doubles the wait.
fn is_startup_hang(error: &LimitsError) -> bool {
    error.to_string().contains("did not answer within")
}

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
    /// Never poll it; see [`Account::live_only`].
    pub live_only: bool,
    /// `team` or `personal`, overriding what the login says.
    pub kind: Option<String>,
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
    if options.live_only && provider != Provider::Claude {
        // A Codex reading names its login only inside a rollout, never in the
        // home, so a live-only Codex account could never learn which is its.
        return Err(LimitsError::Message(
            "--live-only applies to claude accounts only".into(),
        ));
    }
    if options.live_only && options.home.is_none() {
        return Err(LimitsError::Message(
            "--live-only needs --home: the home whose sessions report its numbers".into(),
        ));
    }
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    // Two accounts on one login would each take its live readings and count
    // the same allowance twice.
    if let (Provider::Claude, Some(home)) = (provider, &options.home) {
        if let Some(uuid) = claude::login_account_uuid(home) {
            let holder = registry.accounts.iter().find(|a| {
                a.provider == Provider::Claude
                    && claude::login_account_uuid(&a.poll_home(dirs)).as_deref() == Some(&uuid)
            });
            if let Some(holder) = holder {
                return Err(LimitsError::Message(format!(
                    "{} is already signed in as that login — one account per login",
                    holder.display()
                )));
            }
        }
    }
    let id = if options.live_only {
        registry.next_live_id(provider)
    } else {
        registry.next_id(provider)
    };
    let mut account = Account::new(provider, id, options.label);
    account.home = options.home;
    account.model = options.model;
    account.live_only = options.live_only;
    account.kind = options.kind;
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

/// Pause or resume an account. Pausing drops its snapshot so limits.json
/// (and everything downstream: the feed, the digest) stops naming it; the
/// home and the login stay exactly where they are. Resuming registers it
/// again and the next poll fills it back in.
pub fn pause(dirs: &LimitsDirs, key: &str, paused: bool) -> Result<Account> {
    let path = dirs.config_file();
    let mut registry = require(dirs)?;
    let account = registry.set_paused(key, paused)?;
    registry.save(&path)?;
    if paused {
        for stale in [
            dirs.snapshot_file(&account.id),
            dirs.capture_file(&account.id),
        ] {
            if stale.is_file() {
                std::fs::remove_file(stale)?;
            }
        }
    }
    snapshot::merge(dirs)?;
    Ok(account)
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
    /// Accounts refreshed from a natural session's live reading instead.
    pub live: usize,
    pub failures: usize,
    pub events: Vec<events::Event>,
}

/// Poll the selected accounts, one after another, narrating to stderr.
pub fn poll(dirs: &LimitsDirs, registry: &Registry, selection: &Selection) -> Result<Polled> {
    if let Some(key) = &selection.account {
        if registry.find(key).is_some_and(|a| a.paused) {
            return Err(LimitsError::Message(format!(
                "`{key}` is paused — resume it first: ovm limits resume {key}"
            )));
        }
        if !selection.due_only && registry.find(key).is_some_and(|a| a.live_only) {
            return Err(LimitsError::Message(format!(
                "`{key}` is live-only — its numbers come from the sessions that run in its home, never from a poll"
            )));
        }
    }
    let accounts: Vec<&Account> = registry
        .accounts
        .iter()
        .filter(|a| !a.paused)
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
    let layout = narration_layout(&accounts);
    // Readings natural sessions already produced. Only the background tick
    // uses them: an explicit `ovm limits poll` is a request for a real poll —
    // except for a live-only account, which readings are all it ever gets.
    let wants_live = selection.due_only || accounts.iter().any(|a| a.live_only);
    let readings = if wants_live {
        let mut readings = live::claude_readings(&dirs.live_dir());
        // Only rollouts written since the stalest Codex snapshot can matter.
        let after = registry
            .accounts
            .iter()
            .filter(|a| a.provider == Provider::Codex)
            .map(|a| {
                previous
                    .iter()
                    .find(|s| s.provider == a.provider && s.id == a.id)
                    .map_or(0, |s| s.captured_at)
            })
            .min()
            .unwrap_or(u64::MAX);
        readings.extend(live::codex_readings(&live::default_codex_homes(), after));
        readings
    } else {
        Vec::new()
    };
    for account in accounts {
        let last = previous
            .iter()
            .find(|s| s.provider == account.provider && s.id == account.id);
        // The login a live reading must name: what this account's polls
        // recorded, or — for a Claude account no poll has recorded yet — what
        // its own home holds, so registering an already-signed-in home costs no
        // turn before live readings can keep it fresh.
        let home_login = || {
            (account.provider == Provider::Claude)
                .then(|| claude::login_account_uuid(&account.poll_home(dirs)))
                .flatten()
        };
        // A live-only account has no poll to correct it, so it follows
        // whoever its home is signed in as now — a `/login` there moves it.
        let login = if account.live_only {
            home_login().or_else(|| last.and_then(|l| l.account_id.clone()))
        } else {
            last.and_then(|l| l.account_id.clone()).or_else(home_login)
        };
        let use_live = selection.due_only || account.live_only;
        // Another login's windows are no base for this one's readings: the
        // cross-check would refuse every one of them.
        let base =
            last.filter(|l| !account.live_only || l.account_id.is_none() || l.account_id == login);
        let reading = login.as_deref().filter(|_| use_live).and_then(|login| {
            live::newest_for(
                &readings,
                account.provider,
                login,
                base.map_or(0, |l| l.captured_at),
            )
        });
        if let Some(reading) = reading {
            // After a login switch the old windows are not a "before" either:
            // comparing them would report resets that never happened.
            let prior = base;
            let started;
            let last = match base {
                Some(last) => last,
                None => {
                    started = snapshot::AccountSnapshot {
                        account_id: login.clone(),
                        ..snapshot::AccountSnapshot::for_account(account, live::SOURCE)
                    };
                    &started
                }
            };
            match live::apply(last, reading) {
                Ok(mut merged) => {
                    // A snapshot from before logins were recorded learns it here.
                    merged.account_id = merged.account_id.or_else(|| login.clone());
                    // A Codex rollout may only have refreshed a side meter
                    // (gpt-reserve); then the main `codex` meter still wants
                    // its free poll on its own clock. One that refreshed the
                    // main meter makes the poll pointless.
                    let main_meter_fresh = reading
                        .windows
                        .iter()
                        .any(|w| w.id.starts_with(live::CODEX_MAIN_LIMIT));
                    let due_anyway = account.provider == Provider::Codex
                        && !main_meter_fresh
                        && due::is_due(Some(&poll_clock(last)), registry.interval_minutes, now);
                    if !due_anyway {
                        outcome.live += 1;
                        let noticed = record(dirs, prior, merged, account.signed_in(dirs))?;
                        outcome.events.extend(noticed);
                        continue;
                    }
                }
                Err(why) => eprintln!("  {} live reading ignored: {why}", style("!").yellow()),
            }
        }
        if account.live_only {
            continue;
        }
        if selection.due_only && !due::is_due(last, registry.interval_minutes, now) {
            continue;
        }
        let quiet = selection.due_only && account.provider == Provider::Codex;
        if !quiet {
            narrate_start(account);
        }
        let result = match account.provider {
            Provider::Claude => claude::poll(account, dirs, claude::Timeouts::default()),
            Provider::Codex => poll_codex_with_one_retry(account, dirs),
        };
        let snapshot = match result {
            Ok(snapshot) => {
                if !quiet {
                    narrate(&layout, &snapshot, account);
                }
                snapshot
            }
            Err(error) => {
                outcome.failures += 1;
                let source = match account.provider {
                    Provider::Claude => claude::SOURCE,
                    Provider::Codex => codex::SOURCE,
                };
                let mut failed =
                    snapshot::AccountSnapshot::failed(account, source, error.to_string());
                // The login is still the login: a failed poll must not forget
                // it, or a live reading could never rescue the account.
                failed.account_id = last.and_then(|l| l.account_id.clone());
                narrate(&layout, &failed, account);
                failed
            }
        };
        let noticed = record(dirs, last, snapshot, account.signed_in(dirs))?;
        outcome.polled += 1;
        outcome.events.extend(noticed);
    }
    if outcome.polled == 0 && outcome.live == 0 {
        return Ok(outcome);
    }
    // Announced resets for the public feed: an hourly fetch at most, and a
    // failure only means the last good copy is published again.
    if outcome.polled > 0 {
        crate::announcements::refresh(dirs, now);
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
    // Only a real poll runs the on-poll hooks (publish, digest). Echo refreshes
    // its reading every minute; counting a live merge as a poll would fire the
    // hooks on every five-minute tick — the 288-a-day digest of 2026-09-20.
    if outcome.polled > 0 || !outcome.events.is_empty() {
        hooks::on_poll(dirs, registry, outcome.polled, outcome.events.len());
    }
    Ok(outcome)
}

/// What changed since `last`, with each window's reset memory carried over,
/// written to disk. Shared by real polls and live merges so both notice the
/// same events.
fn record(
    dirs: &LimitsDirs,
    last: Option<&snapshot::AccountSnapshot>,
    mut snapshot: snapshot::AccountSnapshot,
    signed_in: bool,
) -> Result<Vec<events::Event>> {
    let noticed = events::detect(last, &mut snapshot, signed_in);
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
    snapshot::write_snapshot(dirs, &snapshot)?;
    events::append(dirs, &noticed)?;
    Ok(noticed)
}

/// `last` as of its last real poll: a live merge moves `captured_at` forward,
/// but a free Codex poll still runs on its own clock, because a rollout may
/// only have refreshed a secondary meter (gpt-reserve) and not `codex` itself.
fn poll_clock(last: &snapshot::AccountSnapshot) -> snapshot::AccountSnapshot {
    let polled_at = last
        .window_sources
        .values()
        .filter(|source| source.source != live::SOURCE)
        .map(|source| source.at)
        .max();
    let mut clock = last.clone();
    if let Some(at) = polled_at {
        clock.captured_at = at;
    }
    clock
}

/// Change how often an account is polled at most.
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

/// Change how often the digest hook may run. `0` means every poll.
pub fn set_digest_minutes(dirs: &LimitsDirs, minutes: u64) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    let previous = registry.digest_minutes;
    registry.digest_minutes = minutes;
    if let Err(error) = registry.validate() {
        registry.digest_minutes = previous;
        return Err(error);
    }
    registry.save(&path)?;
    Ok(registry)
}

/// Change the order of the account table's rows.
pub fn set_table_sort(dirs: &LimitsDirs, sort: table::SortOrder) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    registry.table_sort = sort;
    registry.save(&path)?;
    Ok(registry)
}

/// Draw the account table as one block per provider, or as one list.
pub fn set_table_grouped(dirs: &LimitsDirs, grouped: bool) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    registry.table_grouped = grouped;
    registry.save(&path)?;
    Ok(registry)
}

/// Record, change or forget an account's plan.
pub fn set_plan(dirs: &LimitsDirs, key: &str, plan: Option<Plan>) -> Result<Account> {
    let path = dirs.config_file();
    let mut registry = require(dirs)?;
    let account = registry.set_plan(key, plan)?;
    registry.save(&path)?;
    Ok(account)
}

/// Set or clear a hook.
pub fn set_hook(dirs: &LimitsDirs, name: &str, command: Option<String>) -> Result<Registry> {
    let path = dirs.config_file();
    let mut registry = Registry::load_or_default(&path)?;
    registry.hooks.set(name, command).ok_or_else(|| {
        LimitsError::Message(format!(
            "no hook called `{name}` — hooks are on-event, on-poll and on-digest"
        ))
    })?;
    registry.save(&path)?;
    Ok(registry)
}

/// Column widths for the poll narration, from every account about to be
/// polled, so the rows line up as they land.
fn narration_layout(accounts: &[&Account]) -> table::Layout {
    let name = accounts
        .iter()
        .map(|a| console::measure_text_width(a.label.as_deref().unwrap_or(&a.id)))
        .max()
        .unwrap_or(0);
    let provider = accounts
        .iter()
        .map(|a| a.provider.to_string().chars().count())
        .max()
        .unwrap_or(0);
    table::Layout::narration(name, provider)
}

/// `  → simcity … ` while a poll runs. Only on a terminal, where
/// [`narrate`] can take the line back; a log gets the finished row alone.
fn narrate_start(account: &Account) {
    if Term::stderr().is_term() {
        eprint!("  {} {} … ", style("→").dim(), account.display());
    }
}

/// One finished poll as a table row: the same columns `ovm limits show`
/// draws, primary window and note.
fn narrate(layout: &table::Layout, snapshot: &snapshot::AccountSnapshot, account: &Account) {
    let term = Term::stderr();
    if term.is_term() {
        let _ = term.clear_line();
    }
    let line = crate::show::table_line(snapshot, account.plan.as_ref());
    // Nothing is cut: a failed poll's whole cause belongs in the log, and a
    // terminal wraps a long line rather than losing it.
    let uncut = usize::MAX;
    eprintln!(
        "  {} {}",
        style("→").dim(),
        table::row(&line, layout, snapshot::now(), uncut)
    );
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
