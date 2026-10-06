//! The account registry: which Claude and Codex logins to poll, and where
//! each one's product home lives.
//!
//! An account is a provider and a slot — `claude-1`, `codex-2` — plus a label
//! you can change whenever you like. The id is assigned once and names the
//! account's home directory, which is what the product keys its stored login
//! to; the label is display and address only. Nothing here is inherited from
//! the homes you work in: every account is registered on purpose and signs in
//! to a home of its own.
//!
//! The registry file is also the activation switch: until an account is added,
//! nothing polls, nothing is written under `~/.ovm/limits`, and no Claude turn
//! is spent.

use crate::hooks::Hooks;
use crate::paths::LimitsDirs;
use crate::plan::Plan;
use crate::table::{Arrangement, SortOrder};
use crate::{LimitsError, Result};
use serde::{Deserialize, Serialize};
use std::fmt;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Provider {
    Claude,
    Codex,
}

impl Provider {
    pub const ALL: [Provider; 2] = [Provider::Claude, Provider::Codex];

    pub fn parse(text: &str) -> Option<Self> {
        match text.to_ascii_lowercase().as_str() {
            "claude" | "cc" => Some(Self::Claude),
            "codex" | "cx" => Some(Self::Codex),
            _ => None,
        }
    }

    pub fn display_name(self) -> &'static str {
        match self {
            Self::Claude => "Claude Code",
            Self::Codex => "Codex",
        }
    }

    /// The environment variable that relocates this product's home.
    pub fn home_env(self) -> &'static str {
        match self {
            Self::Claude => "CLAUDE_CONFIG_DIR",
            Self::Codex => "CODEX_HOME",
        }
    }

    /// Is this a home the person's own `claude` / `codex` sessions use? Polls
    /// and logins refuse it.
    ///
    /// Every candidate is checked, not only the one this process's environment
    /// selects. A `CODEX_HOME` exported in one shell must not make the real
    /// `~/.codex` look safe, and the launchd agent runs with none of the
    /// shell's overrides, so the two would otherwise disagree about the same
    /// account. `$HOME` itself is refused because Claude Code keeps
    /// `.claude.json` BESIDE `~/.claude`: a home of `$HOME` reaches the real
    /// file without ever being the default home.
    pub fn is_default_home(self, path: &Path) -> bool {
        let mut candidates: Vec<PathBuf> = Vec::new();
        if let Some(value) = std::env::var_os(self.home_env()).filter(|v| !v.is_empty()) {
            candidates.push(PathBuf::from(value));
        }
        if let Some(home) = dirs::home_dir() {
            candidates.push(home.join(match self {
                Self::Claude => ".claude",
                Self::Codex => ".codex",
            }));
            candidates.push(home);
        }
        let path = resolve(path);
        candidates
            .iter()
            .any(|candidate| resolve(candidate) == path)
    }
}

/// A path that can be compared with another.
///
/// `canonicalize` alone is not enough: it fails on a path that does not exist
/// yet, and `~/absent/../.claude` then compares unequal to `~/.claude` right up
/// until `create_dir_all` makes them the same directory. Folding `.` and `..`
/// away first is not enough either, because `..` after a symlink means the
/// target's parent, not the parent it is spelled under: with
/// `/work/link -> ~/project`, `/work/link/..` is `~`, not `/work`.
///
/// So each `..` is applied to the real directory reached so far, and whatever
/// ancestors exist are resolved even when the final component does not — a
/// home directory reached through a symlink must not look like a different
/// place merely because the home it names has yet to be created.
fn resolve(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for part in path.components() {
        match part {
            std::path::Component::ParentDir => {
                out = out.canonicalize().unwrap_or(out);
                out.pop();
            }
            std::path::Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    if let Ok(canonical) = out.canonicalize() {
        return canonical;
    }
    match (out.parent(), out.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(parent) => parent.join(name),
            Err(_) => out,
        },
        _ => out,
    }
}

impl fmt::Display for Provider {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Claude => "claude",
            Self::Codex => "codex",
        })
    }
}

/// An account id becomes a directory name and a snapshot file name verbatim,
/// so it is restricted to characters that cannot collide or escape: two ids
/// that differ must land in two files, or one account's poll history throttles
/// the other's. Ids are assigned by [`Registry::next_id`], never typed; this
/// guards a hand-edited file.
pub fn validate_id(id: &str) -> Result<()> {
    let ok = !id.is_empty()
        && id.len() <= 64
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_');
    if ok {
        Ok(())
    } else {
        Err(LimitsError::Message(format!(
            "account id `{id}` must be 1–64 characters of A–Z, a–z, 0–9, `-` or `_`"
        )))
    }
}

/// A label is yours to choose and to change, so it only has to be printable,
/// one line, and short enough to sit in a table. It never reaches the
/// filesystem — that is the id's job — so renaming cannot orphan a login.
pub fn validate_label(label: &str) -> Result<()> {
    let trimmed = label.trim();
    let ok = !trimmed.is_empty()
        && trimmed.chars().count() <= 40
        && !trimmed.chars().any(|c| c.is_control());
    if ok {
        Ok(())
    } else {
        Err(LimitsError::Message(format!(
            "label `{label}` must be 1–40 printable characters on one line"
        )))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Account {
    /// Assigned once, at creation, and never changed: `claude-1`, `codex-2`.
    /// It names this account's home directory and its snapshot file, and the
    /// product keys its stored credential to that home — so an id that moved
    /// would orphan the login. The label is the part you rename.
    pub id: String,
    pub provider: Provider,
    /// What you call this account. Optional, free to change, and accepted
    /// anywhere an id is.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// This login's product home (`CLAUDE_CONFIG_DIR` / `CODEX_HOME`).
    /// `None` means the home `ovm limits` keeps for the account under its own
    /// directory. It never means the product's default home: polling the
    /// login you work in signs that login out.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub home: Option<PathBuf>,
    /// Claude only: the model the one-word poll prompt is sent to. The
    /// cheapest one is the right one; the reply is discarded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Set aside, not forgotten: a paused account keeps its home and its
    /// login, but no poll touches it and the merged view leaves it out. For
    /// a subscription that lapsed and is coming back (2026-09-19).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub paused: bool,
    /// Never polled: its numbers come only from the sessions that run in its
    /// home (Echo's live readings). For a home someone works in — an
    /// `ovm run` account folder — where a poll would refresh the very login
    /// those sessions hold and sign them out.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub live_only: bool,
    /// `team` or `personal` when the person says so; otherwise it is read
    /// from the login (see [`crate::claude::login_details`]).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// The subscription as the person recorded it with `ovm limits plan`:
    /// live or cancelled, and the day it ends or renews. No product reports
    /// this, so it is typed in. Missing means nothing was recorded.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<Plan>,
}

impl Account {
    pub fn new(provider: Provider, id: impl Into<String>, label: Option<String>) -> Self {
        Self {
            id: id.into(),
            provider,
            label,
            home: None,
            model: None,
            paused: false,
            live_only: false,
            kind: None,
            plan: None,
        }
    }

    /// How this account is written for a person: `claude-1`, or
    /// `claude-1 (work)` once it has a label.
    pub fn display(&self) -> String {
        match &self.label {
            Some(label) => format!("{} ({label})", self.id),
            None => self.id.clone(),
        }
    }

    /// Does this account answer to what the person typed? Either its id or its
    /// label will do, since the id is what they see when there is no label.
    pub fn answers_to(&self, key: &str) -> bool {
        self.id.eq_ignore_ascii_case(key)
            || self
                .label
                .as_deref()
                .is_some_and(|label| label.eq_ignore_ascii_case(key))
    }

    /// Has this account's home actually been signed in to? Neither the
    /// directory nor a `.claude.json` is evidence — a poll creates both, so a
    /// home that has only ever failed to poll would read as ready. The account
    /// record the product itself writes at login is.
    pub fn signed_in(&self, dirs: &LimitsDirs) -> bool {
        let home = self.poll_home(dirs);
        match self.provider {
            Provider::Claude => std::fs::read_to_string(home.join(".claude.json"))
                .ok()
                .and_then(|raw| serde_json::from_str::<serde_json::Value>(&raw).ok())
                .is_some_and(|root| root.get("oauthAccount").is_some()),
            Provider::Codex => home.join("auth.json").is_file(),
        }
    }

    /// The home this account's polls run in: its own by default, or the one
    /// configured explicitly.
    pub fn poll_home(&self, dirs: &LimitsDirs) -> PathBuf {
        self.home
            .clone()
            .unwrap_or_else(|| dirs.poll_home(&self.id))
    }

    /// Is the home one this tool made for the account, and so may delete
    /// with it? An explicit `--home` belongs to the person.
    pub fn owns_home(&self) -> bool {
        self.home.is_none()
    }
}

pub const DEFAULT_CLAUDE_MODEL: &str = "haiku";
pub const DEFAULT_INTERVAL_MINUTES: u64 = 60;

pub(crate) fn default_interval_minutes() -> u64 {
    DEFAULT_INTERVAL_MINUTES
}

/// How often the digest hook may run. Separate from the poll interval on
/// purpose: how fresh the numbers are and how often a phone buzzes are two
/// different questions, and tying them together is what turned an "hourly"
/// digest into one every five minutes.
pub const DEFAULT_DIGEST_MINUTES: u64 = 60;

pub(crate) fn default_digest_minutes() -> u64 {
    DEFAULT_DIGEST_MINUTES
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// How often a Claude account is worth a paid poll. The background agent
    /// ticks far more often than this and only spends a turn once the interval
    /// has passed or a window has reset since the last snapshot.
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u64,
    /// How often the `on_digest` hook may run, at most. Every other hook runs
    /// when its event happens; this one is a heartbeat to a phone, so it is
    /// rate-limited rather than triggered.
    #[serde(default = "default_digest_minutes")]
    pub digest_minutes: u64,
    /// The order of the account table's rows, on every surface that draws it.
    #[serde(default)]
    pub table_sort: SortOrder,
    /// Draw the account table as one block per provider.
    #[serde(default)]
    pub table_grouped: bool,
    /// Commands to run when something happens; see `hooks`.
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
    /// Read only, never written: where the first builds of live-only accounts
    /// kept them, inside this file. A build from before them rewrites this
    /// file with only the fields it knows, so they now live in
    /// [`LIVE_ACCOUNTS_FILE`] instead; see [`Registry::save`].
    #[serde(default, skip_serializing)]
    pub(crate) live_accounts: Vec<Account>,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            digest_minutes: DEFAULT_DIGEST_MINUTES,
            table_sort: SortOrder::default(),
            table_grouped: false,
            hooks: Hooks::default(),
            live_accounts: Vec::new(),
        }
    }
}

/// `15m`, `1h`, `2h30m`, or plain minutes → minutes.
pub fn parse_interval(text: &str) -> Result<u64> {
    let text = text.trim().to_ascii_lowercase();
    let bad = || {
        LimitsError::Message(format!(
            "`{text}` is not an interval — try 15m, 1h, or 2h30m"
        ))
    };
    if text.is_empty() {
        return Err(bad());
    }
    if let Ok(minutes) = text.parse::<u64>() {
        return Ok(minutes);
    }
    let mut total = 0u64;
    let mut number = String::new();
    for c in text.chars() {
        match c {
            '0'..='9' => number.push(c),
            'h' | 'm' => {
                let n: u64 = number.parse().map_err(|_| bad())?;
                number.clear();
                total += if c == 'h' { n * 60 } else { n };
            }
            _ => return Err(bad()),
        }
    }
    if !number.is_empty() {
        return Err(bad());
    }
    Ok(total)
}

/// `15m`, `1h`, `2h 30m` — the interval as a person would say it.
pub fn interval_label(minutes: u64) -> String {
    match (minutes / 60, minutes % 60) {
        (0, m) => format!("{m}m"),
        (h, 0) => format!("{h}h"),
        (h, m) => format!("{h}h {m}m"),
    }
}

/// The live-only accounts, beside `config.json`. A build from before live-only
/// accounts never reads this file, so it never polls them, and never writes
/// it, so a config change made with that build (add, pause, interval) cannot
/// drop them.
pub const LIVE_ACCOUNTS_FILE: &str = "live-accounts.json";

#[derive(Debug, Default, Serialize, Deserialize)]
struct LiveAccountsFile {
    #[serde(default)]
    accounts: Vec<Account>,
}

fn live_accounts_path(config: &Path) -> PathBuf {
    config.with_file_name(LIVE_ACCOUNTS_FILE)
}

fn read_live_accounts(path: &Path) -> Result<Vec<Account>> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    let file: LiveAccountsFile = serde_json::from_str(&raw).map_err(|error| {
        LimitsError::Message(format!(
            "{} is not a valid live-accounts file ({error})",
            path.display()
        ))
    })?;
    Ok(file.accounts)
}

impl Registry {
    /// Bounds on what the agent may be told to do. Below five minutes every
    /// tick would pay; above a week the numbers stop meaning anything.
    pub const MIN_INTERVAL_MINUTES: u64 = 5;
    pub const MAX_INTERVAL_MINUTES: u64 = 7 * 24 * 60;

    /// `None` when no account has ever been added — the file is the switch.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let live_path = live_accounts_path(path);
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) if !raw.trim().is_empty() => raw,
            Ok(_) => String::new(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(error.into()),
        };
        // The first save of a live account writes its file before config.json
        // exists; after a crash between the two, the live file alone still
        // counts, or the next add would start empty and overwrite it.
        let raw = if !raw.is_empty() {
            raw
        } else if live_path.exists() {
            "{}".to_string()
        } else {
            return Ok(None);
        };
        let mut registry: Self = serde_json::from_str(&raw).map_err(|error| {
            let shown = path.display();
            // A file from before accounts had ids names them instead. It was
            // never carried forward on purpose: the homes it points at were
            // shared with the person's own sessions, which is what the
            // registry exists to stop.
            if raw.contains("\"name\"") && !raw.contains("\"id\"") {
                LimitsError::Message(format!(
                    "{shown} predates the account registry — run `ovm limits uninstall --yes`, \
                     then `ovm limits` to add the accounts again"
                ))
            } else {
                LimitsError::Message(format!("{shown} is not a valid limits registry ({error})"))
            }
        })?;
        // The file before the old in-config field: after a crash mid-save the
        // file holds the newer copy of an account that is in both.
        let mut live = read_live_accounts(&live_path)?;
        for account in std::mem::take(&mut registry.live_accounts) {
            if !live.iter().any(|a| a.id.eq_ignore_ascii_case(&account.id)) {
                live.push(account);
            }
        }
        for mut account in live {
            account.live_only = true;
            registry.admit_live(account);
        }
        registry
            .validate()
            .map_err(|error| LimitsError::Message(format!("{}: {error}", path.display())))?;
        Ok(Some(registry))
    }

    /// How the account table is laid out, as last chosen.
    pub fn arrangement(&self) -> Arrangement {
        Arrangement {
            sort: self.table_sort,
            grouped: self.table_grouped,
        }
    }

    /// Whatever is on disk, or an empty registry about to be written.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        Ok(Self::load(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut on_disk = self.clone();
        let (live, polled): (Vec<Account>, Vec<Account>) =
            on_disk.accounts.drain(..).partition(|a| a.live_only);
        on_disk.accounts = polled;
        // The live file first: a crash between the two writes leaves the
        // accounts in both places, which load folds together, rather than in
        // neither.
        let live_path = live_accounts_path(path);
        if live.is_empty() {
            match std::fs::remove_file(&live_path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
        } else {
            let json = serde_json::to_string_pretty(&LiveAccountsFile { accounts: live })?;
            crate::snapshot::write_atomic(&live_path, json.as_bytes())?;
        }
        let json = serde_json::to_string_pretty(&on_disk)?;
        crate::snapshot::write_atomic(path, json.as_bytes())
    }

    /// Take in a live-only account read from disk. An older build adds and
    /// renames accounts without seeing the live ones, so what it writes can
    /// hold a live account's id (one migrated from inside config.json kept
    /// its `claude-N`) or use its id or label as a label. The polled account
    /// keeps what it has; the live one takes a fresh `claude-live-N` id or
    /// gives up its label, rather than being lost or the whole registry
    /// refusing to load.
    fn admit_live(&mut self, mut account: Account) {
        if self.names_taken(&account.id) {
            let fresh = self.next_live_id(account.provider);
            eprintln!(
                "ovm limits: live-only account {} is now {fresh}; an account added by an \
                 older build uses that name",
                account.id
            );
            account.id = fresh;
        }
        if let Some(label) = &account.label {
            if self.names_taken(label) {
                eprintln!(
                    "ovm limits: live-only account {} lost its label `{label}` to an account \
                     added by an older build; `ovm limits rename {} <label>` gives it a new one",
                    account.id, account.id
                );
                account.label = None;
            }
        }
        self.accounts.push(account);
    }

    /// Whether `name` already selects an account, as its id or its label.
    fn names_taken(&self, name: &str) -> bool {
        self.accounts.iter().any(|other| {
            other.id.eq_ignore_ascii_case(name)
                || other
                    .label
                    .as_deref()
                    .is_some_and(|label| label.eq_ignore_ascii_case(name))
        })
    }

    /// The account the person meant, by id or by label.
    pub fn find(&self, key: &str) -> Option<&Account> {
        self.accounts.iter().find(|account| account.answers_to(key))
    }

    /// The next free id for a product: `claude-1`, then `claude-2`. Counting
    /// from the ids in use rather than the number of accounts, so removing
    /// `claude-1` cannot hand its name — and its old home — to a new account
    /// while a `claude-2` still exists.
    pub fn next_id(&self, provider: Provider) -> String {
        self.next_id_with(provider, "")
    }

    /// The next free id for a live-only account: `claude-live-1`. Its own
    /// sequence, because a build from before live-only accounts cannot see
    /// them and would hand out `claude-N` ids they already hold.
    pub fn next_live_id(&self, provider: Provider) -> String {
        self.next_id_with(provider, "live-")
    }

    fn next_id_with(&self, provider: Provider, infix: &str) -> String {
        (1..)
            .map(|n| format!("{provider}-{infix}{n}"))
            .find(|candidate| !self.names_taken(candidate))
            .expect("an unbounded sequence contains a free id")
    }

    /// Register an account the caller shaped (`Account::new` with the id from
    /// [`Registry::next_id`], plus any `--home` or `--model`).
    pub fn push(&mut self, account: Account) -> Result<&Account> {
        self.accounts.push(account);
        // One rule, in one place: the file's invariants. A rejected account is
        // taken straight back out, so a failed add leaves nothing behind.
        if let Err(error) = self.validate() {
            self.accounts.pop();
            return Err(error);
        }
        Ok(self.accounts.last().expect("just pushed"))
    }

    /// Give an account a new label, or take its label away. The id, its home
    /// and its stored login are untouched — that is the whole point of having
    /// an id.
    pub fn relabel(&mut self, key: &str, label: Option<String>) -> Result<Account> {
        let index = self.index_of(key)?;
        let previous = self.accounts[index].label.take();
        self.accounts[index].label = label;
        if let Err(error) = self.validate() {
            self.accounts[index].label = previous;
            return Err(error);
        }
        Ok(self.accounts[index].clone())
    }

    pub fn set_paused(&mut self, key: &str, paused: bool) -> Result<Account> {
        let index = self.index_of(key)?;
        self.accounts[index].paused = paused;
        Ok(self.accounts[index].clone())
    }

    /// Record, change or forget an account's plan. Returns the account as
    /// it now stands.
    pub fn set_plan(&mut self, key: &str, plan: Option<Plan>) -> Result<Account> {
        let index = self.index_of(key)?;
        self.accounts[index].plan = plan;
        Ok(self.accounts[index].clone())
    }

    pub fn remove(&mut self, key: &str) -> Result<Account> {
        let index = self.index_of(key)?;
        Ok(self.accounts.remove(index))
    }

    fn index_of(&self, key: &str) -> Result<usize> {
        self.accounts
            .iter()
            .position(|account| account.answers_to(key))
            .ok_or_else(|| {
                LimitsError::Message(format!("no account `{key}` — see: ovm limits list"))
            })
    }

    /// The invariants the file must hold, whoever wrote it: ids are file-name
    /// safe and unique, labels are unique and never look like someone's id,
    /// the interval is sane. Comparisons ignore case: the default macOS volume
    /// folds it, so `claude-1` and `Claude-1` would share one home.
    pub fn validate(&self) -> Result<()> {
        if !(Self::MIN_INTERVAL_MINUTES..=Self::MAX_INTERVAL_MINUTES)
            .contains(&self.interval_minutes)
        {
            return Err(LimitsError::Message(format!(
                "interval_minutes must be between {} and {} (got {})",
                Self::MIN_INTERVAL_MINUTES,
                Self::MAX_INTERVAL_MINUTES,
                self.interval_minutes
            )));
        }
        // Zero is meaningful here — "every poll", the old behaviour — so only
        // the upper bound is checked.
        if self.digest_minutes > Self::MAX_INTERVAL_MINUTES {
            return Err(LimitsError::Message(format!(
                "digest_minutes must be at most {} (got {})",
                Self::MAX_INTERVAL_MINUTES,
                self.digest_minutes
            )));
        }
        for (index, account) in self.accounts.iter().enumerate() {
            validate_id(&account.id)?;
            if self.accounts[..index]
                .iter()
                .any(|other| other.id.eq_ignore_ascii_case(&account.id))
            {
                return Err(LimitsError::Message(format!(
                    "account id {} is listed twice",
                    account.id
                )));
            }
            let Some(label) = &account.label else {
                continue;
            };
            validate_label(label)?;
            // A label selects an account on the command line, the same way an
            // id does — so it may not be another account's label, and it may
            // not be anyone's id, or typing it would reach the wrong account.
            if self
                .accounts
                .iter()
                .any(|other| other.id.eq_ignore_ascii_case(label))
            {
                return Err(LimitsError::Message(format!(
                    "`{label}` is an account id, so it cannot be a label"
                )));
            }
            if self.accounts[..index].iter().any(|other| {
                other
                    .label
                    .as_deref()
                    .is_some_and(|l| l.eq_ignore_ascii_case(label))
            }) {
                return Err(LimitsError::Message(format!(
                    "two accounts are labelled `{label}`"
                )));
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    trait Add {
        fn add(&mut self, provider: Provider, label: Option<String>) -> Result<&Account>;
    }

    impl Add for Registry {
        fn add(&mut self, provider: Provider, label: Option<String>) -> Result<&Account> {
            let account = Account::new(provider, self.next_id(provider), label);
            self.push(account)
        }
    }

    fn registry() -> Registry {
        let mut registry = Registry::default();
        registry.add(Provider::Claude, Some("work".into())).unwrap();
        registry.add(Provider::Codex, None).unwrap();
        registry
    }

    #[test]
    fn every_home_the_person_works_in_is_refused_however_it_is_spelled() {
        let home = dirs::home_dir().expect("a home directory");
        assert!(Provider::Claude.is_default_home(&home.join(".claude")));
        assert!(Provider::Codex.is_default_home(&home.join(".codex")));
        // `$HOME` itself: Claude Code keeps `.claude.json` beside `~/.claude`,
        // so a home of `$HOME` reaches the real file.
        assert!(Provider::Claude.is_default_home(&home));
        // A path that only becomes the default home once `..` is folded — the
        // shape `canonicalize` alone lets through, because it does not exist
        // until `create_dir_all` runs.
        assert!(Provider::Codex.is_default_home(&home.join("absent").join("..").join(".codex")));
        assert!(!Provider::Claude.is_default_home(&home.join(".ovm/limits/homes/claude-1")));
    }

    #[test]
    fn dot_dot_means_what_the_filesystem_says_it_means_not_what_the_spelling_says() {
        let home = dirs::home_dir().expect("a home directory");
        let temp = tempfile::tempdir().unwrap();
        // `link` points into the home directory, so `link/..` IS the home
        // directory — and a home spelled that way reaches ~/.claude.json.
        let link = temp.path().join("link");
        std::os::unix::fs::symlink(home.join(".ovm"), &link).unwrap();
        assert!(Provider::Claude.is_default_home(&link.join("..")));
        let plain = temp.path().join("plain");
        std::fs::create_dir(&plain).unwrap();
        assert!(!Provider::Claude.is_default_home(&plain.join("..")));
    }

    #[test]
    fn ids_count_up_per_product_and_a_gap_is_filled_before_the_sequence_grows() {
        let mut registry = registry();
        assert_eq!(registry.accounts[0].id, "claude-1");
        assert_eq!(registry.accounts[1].id, "codex-1");
        registry.add(Provider::Claude, None).unwrap();
        assert_eq!(registry.accounts[2].id, "claude-2");
        registry.remove("claude-1").unwrap();
        assert_eq!(registry.next_id(Provider::Claude), "claude-1");
        assert_eq!(registry.next_id(Provider::Codex), "codex-2");
    }

    #[test]
    fn an_account_answers_to_its_id_or_its_label_ignoring_case() {
        let registry = registry();
        assert_eq!(registry.find("WORK").unwrap().id, "claude-1");
        assert_eq!(registry.find("Claude-1").unwrap().id, "claude-1");
        assert!(registry.find("codex-1").unwrap().label.is_none());
        assert!(registry.find("codex-2").is_none());
    }

    #[test]
    fn renaming_changes_the_label_and_nothing_else() {
        let mut registry = registry();
        let before = registry.find("claude-1").unwrap().clone();
        let after = registry.relabel("work", Some("personal".into())).unwrap();
        assert_eq!(after.id, before.id);
        assert_eq!(after.display(), "claude-1 (personal)");
        assert!(registry.find("work").is_none());
        assert!(registry.find("personal").is_some());
        registry.relabel("claude-1", None).unwrap();
        assert_eq!(registry.find("claude-1").unwrap().display(), "claude-1");
    }

    #[test]
    fn a_label_may_not_be_taken_and_may_not_look_like_an_id() {
        let mut registry = registry();
        let error = registry
            .add(Provider::Codex, Some("WORK".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("two accounts are labelled"), "{error}");
        assert_eq!(registry.accounts.len(), 2, "rolled back");
        let error = registry
            .add(Provider::Codex, Some("claude-1".into()))
            .unwrap_err()
            .to_string();
        assert!(error.contains("is an account id"), "{error}");
        // A label that matches the account's OWN id is refused too: it would
        // be pointless at best and misleading once ids are renumbered.
        assert!(registry.relabel("codex-1", Some("CODEX-1".into())).is_err());
        assert!(
            registry.find("codex-1").unwrap().label.is_none(),
            "rolled back"
        );
    }

    #[test]
    fn labels_are_bounded_printable_single_lines() {
        let mut registry = registry();
        for bad in ["", "   ", "a\nb", "a".repeat(41).as_str()] {
            assert!(
                registry.add(Provider::Claude, Some(bad.into())).is_err(),
                "{bad:?} must be rejected"
            );
        }
        assert_eq!(registry.accounts.len(), 2);
        registry
            .add(Provider::Claude, Some("über cool 🐱".into()))
            .unwrap();
    }

    #[test]
    fn the_registry_round_trips_through_json() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let mut registry = registry();
        registry
            .push(Account {
                id: "claude-2".into(),
                provider: Provider::Claude,
                label: Some("elsewhere".into()),
                home: Some(PathBuf::from("/tmp/claude-elsewhere")),
                model: Some("haiku".into()),
                paused: false,
                live_only: false,
                kind: None,
                plan: None,
            })
            .unwrap();
        registry.save(&path).unwrap();
        assert_eq!(Registry::load(&path).unwrap().unwrap(), registry);
    }

    #[test]
    fn a_plan_round_trips_and_an_account_without_one_writes_no_key() {
        use crate::plan::{PlanDate, PlanStatus};
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        // A registry from before plans: the account loads with none, and
        // saving it back writes no `plan` key.
        std::fs::write(
            &path,
            r#"{"accounts": [{"id": "codex-1", "provider": "codex", "label": "spare"}]}"#,
        )
        .unwrap();
        let mut registry = Registry::load(&path).unwrap().unwrap();
        assert_eq!(registry.accounts[0].plan, None);
        registry.save(&path).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert!(raw["accounts"][0].get("plan").is_none());

        let plan = Plan {
            name: Some("Example Pro 100".into()),
            status: PlanStatus::Cancelled,
            ends_on: PlanDate::parse("2026-10-30"),
        };
        let account = registry.set_plan("spare", Some(plan.clone())).unwrap();
        assert_eq!(account.plan.as_ref(), Some(&plan));
        registry.save(&path).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(
            raw["accounts"][0]["plan"],
            serde_json::json!({
                "name": "Example Pro 100",
                "status": "cancelled",
                "ends_on": "2026-10-30"
            })
        );
        let loaded = Registry::load(&path).unwrap().unwrap();
        assert_eq!(loaded, registry);

        registry.set_plan("codex-1", None).unwrap();
        assert_eq!(registry.accounts[0].plan, None);
        assert!(registry.set_plan("nobody", None).is_err());
    }

    #[test]
    fn the_table_arrangement_round_trips_and_defaults_when_missing() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        // A registry from before the setting reads as provider order, flat.
        std::fs::write(&path, r#"{"accounts": [], "interval_minutes": 30}"#).unwrap();
        let old = Registry::load(&path).unwrap().unwrap();
        assert_eq!(old.table_sort, SortOrder::Provider);
        assert!(!old.table_grouped);
        assert_eq!(old.arrangement(), Arrangement::default());

        let mut registry = registry();
        registry.table_sort = SortOrder::Reset;
        registry.table_grouped = true;
        registry.save(&path).unwrap();
        let raw: serde_json::Value =
            serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(raw["table_sort"], "reset");
        assert_eq!(raw["table_grouped"], true);
        let loaded = Registry::load(&path).unwrap().unwrap();
        assert_eq!(loaded, registry);
        assert_eq!(
            loaded.arrangement(),
            Arrangement {
                sort: SortOrder::Reset,
                grouped: true,
            }
        );
    }

    #[test]
    fn no_file_and_an_empty_file_both_mean_nothing_registered() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        assert!(Registry::load(&path).unwrap().is_none());
        std::fs::write(&path, "\n").unwrap();
        assert!(Registry::load(&path).unwrap().is_none());
        assert!(Registry::load_or_default(&path)
            .unwrap()
            .accounts
            .is_empty());
    }

    #[test]
    fn a_file_from_before_ids_is_named_for_what_it_is() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"accounts": [{"name": "default", "provider": "claude"}], "interval_minutes": 60}"#,
        )
        .unwrap();
        let error = Registry::load(&path).unwrap_err().to_string();
        assert!(error.contains("predates the account registry"), "{error}");
    }

    #[test]
    fn a_hand_edited_file_is_validated_on_load() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        for (body, expected) in [
            (
                r#"{"accounts": [{"id": "work/a", "provider": "claude"}]}"#,
                "must be 1–64",
            ),
            (
                r#"{"accounts": [{"id": "a", "provider": "claude"}, {"id": "A", "provider": "claude"}]}"#,
                "listed twice",
            ),
            (
                r#"{"accounts": [{"id": "claude-1", "provider": "claude"}, {"id": "codex-1", "provider": "codex", "label": "Claude-1"}]}"#,
                "is an account id",
            ),
            (
                r#"{"accounts": [], "interval_minutes": 0}"#,
                "interval_minutes",
            ),
        ] {
            std::fs::write(&path, body).unwrap();
            let error = Registry::load(&path).unwrap_err().to_string();
            assert!(error.contains(expected), "{body}: {error}");
        }
        std::fs::write(&path, r#"{"accounts": [], "interval_minutes": 30}"#).unwrap();
        assert_eq!(Registry::load(&path).unwrap().unwrap().interval_minutes, 30);
    }

    #[test]
    fn intervals_parse_and_print_the_way_people_say_them() {
        assert_eq!(parse_interval("15m").unwrap(), 15);
        assert_eq!(parse_interval("1h").unwrap(), 60);
        assert_eq!(parse_interval("2h30m").unwrap(), 150);
        assert_eq!(parse_interval("45").unwrap(), 45);
        assert!(parse_interval("soon").is_err());
        assert!(parse_interval("1h5").is_err());
        assert_eq!(interval_label(15), "15m");
        assert_eq!(interval_label(60), "1h");
        assert_eq!(interval_label(150), "2h 30m");
    }

    #[test]
    fn hooks_round_trip_and_stay_out_of_the_file_when_empty() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        let mut registry = registry();
        registry.save(&path).unwrap();
        assert!(!std::fs::read_to_string(&path).unwrap().contains("hooks"));
        registry.hooks.on_event = Some("true".into());
        registry.save(&path).unwrap();
        assert_eq!(Registry::load(&path).unwrap().unwrap(), registry);
    }

    #[test]
    fn provider_aliases_parse() {
        assert_eq!(Provider::parse("cc"), Some(Provider::Claude));
        assert_eq!(Provider::parse("Codex"), Some(Provider::Codex));
        assert_eq!(Provider::parse("gemini"), None);
    }

    /// A registry with one polled account (`claude-1`, "main") and one
    /// live-only seat (`claude-live-1`, "client"), saved to `config.json`.
    fn saved_with_a_live_seat(dir: &Path) -> (PathBuf, Registry) {
        let path = dir.join("config.json");
        let mut registry = Registry::default();
        registry
            .push(Account::new(
                Provider::Claude,
                "claude-1",
                Some("main".into()),
            ))
            .unwrap();
        let mut seat = Account::new(
            Provider::Claude,
            registry.next_live_id(Provider::Claude),
            Some("client".into()),
        );
        seat.home = Some(dir.join("client"));
        seat.live_only = true;
        registry.push(seat).unwrap();
        registry.save(&path).unwrap();
        (path, registry)
    }

    fn read_json(path: &Path) -> serde_json::Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn live_only_accounts_are_stored_where_an_older_build_cannot_poll_them() {
        let temp = tempfile::tempdir().unwrap();
        let (path, registry) = saved_with_a_live_seat(temp.path());

        let config = read_json(&path);
        let polled: Vec<&str> = config["accounts"]
            .as_array()
            .unwrap()
            .iter()
            .map(|a| a["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            polled,
            ["claude-1"],
            "an older build polls every entry here"
        );
        assert!(config.get("live_accounts").is_none());
        let live = read_json(&temp.path().join(LIVE_ACCOUNTS_FILE));
        assert_eq!(live["accounts"][0]["id"], "claude-live-1");

        let loaded = Registry::load(&path).unwrap().unwrap();
        assert_eq!(loaded, registry);
        assert_eq!(loaded.next_id(Provider::Claude), "claude-2");
        assert_eq!(loaded.next_live_id(Provider::Claude), "claude-live-2");
    }

    #[test]
    fn an_older_build_rewriting_config_json_keeps_the_live_accounts() {
        let temp = tempfile::tempdir().unwrap();
        let (path, _) = saved_with_a_live_seat(temp.path());

        // What 0.1.11 does on any change: read the fields it knows, add its
        // own account with its own next id, write only those fields back.
        let mut config = read_json(&path);
        config["accounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "claude-2", "provider": "claude", "label": "client"}));
        config["interval_minutes"] = serde_json::json!(30);
        std::fs::write(&path, serde_json::to_string_pretty(&config).unwrap()).unwrap();

        let loaded = Registry::load(&path).unwrap().unwrap();
        let seat = loaded.find("claude-live-1").expect("the seat survives");
        assert!(seat.live_only);
        assert_eq!(
            seat.label, None,
            "the older build's account took the label; the registry still loads"
        );
        assert_eq!(loaded.find("client").unwrap().id, "claude-2");
        assert_eq!(loaded.interval_minutes, 30);
    }

    #[test]
    fn live_accounts_from_inside_config_json_move_to_their_own_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"accounts":[{"id":"claude-1","provider":"claude"}],
                "live_accounts":[{"id":"claude-2","provider":"claude","home":"/tmp/seat"}]}"#,
        )
        .unwrap();

        let loaded = Registry::load(&path).unwrap().unwrap();
        assert!(loaded.find("claude-2").unwrap().live_only);
        loaded.save(&path).unwrap();

        assert!(read_json(&path).get("live_accounts").is_none());
        let live = read_json(&temp.path().join(LIVE_ACCOUNTS_FILE));
        assert_eq!(live["accounts"][0]["id"], "claude-2");
        assert_eq!(Registry::load(&path).unwrap().unwrap(), loaded);
    }

    #[test]
    fn a_live_account_in_both_places_is_taken_once_and_the_last_one_removes_the_file() {
        let temp = tempfile::tempdir().unwrap();
        let (path, registry) = saved_with_a_live_seat(temp.path());
        // A crash after the live file was written but before config.json was:
        // the seat is also still in the old in-config field.
        let mut config = read_json(&path);
        config["live_accounts"] =
            read_json(&temp.path().join(LIVE_ACCOUNTS_FILE))["accounts"].clone();
        std::fs::write(&path, config.to_string()).unwrap();
        let mut loaded = Registry::load(&path).unwrap().unwrap();
        assert_eq!(loaded, registry);

        loaded.accounts.retain(|a| !a.live_only);
        loaded.save(&path).unwrap();
        assert!(!temp.path().join(LIVE_ACCOUNTS_FILE).exists());
    }

    #[test]
    fn a_migrated_live_account_whose_id_an_older_build_reused_takes_a_fresh_one() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("config.json");
        std::fs::write(
            &path,
            r#"{"accounts":[{"id":"claude-1","provider":"claude"}],
                "live_accounts":[{"id":"claude-2","provider":"claude","home":"/tmp/seat","label":"client"}]}"#,
        )
        .unwrap();
        Registry::load(&path).unwrap().unwrap().save(&path).unwrap();

        // 0.1.11 sees only claude-1, so its next account is claude-2.
        let mut config = read_json(&path);
        config["accounts"]
            .as_array_mut()
            .unwrap()
            .push(serde_json::json!({"id": "claude-2", "provider": "claude"}));
        std::fs::write(&path, config.to_string()).unwrap();

        let loaded = Registry::load(&path).unwrap().unwrap();
        assert!(!loaded.find("claude-2").unwrap().live_only);
        let seat = loaded
            .find("client")
            .expect("the seat survives under its label");
        assert!(seat.live_only);
        assert_eq!(seat.id, "claude-live-1");
        loaded.save(&path).unwrap();
        assert_eq!(Registry::load(&path).unwrap().unwrap(), loaded);
    }

    #[test]
    fn an_older_build_labelling_an_account_with_a_live_id_does_not_break_loading() {
        let temp = tempfile::tempdir().unwrap();
        let (path, _) = saved_with_a_live_seat(temp.path());
        let mut config = read_json(&path);
        config["accounts"][0]["label"] = serde_json::json!("claude-live-1");
        std::fs::write(&path, config.to_string()).unwrap();

        let loaded = Registry::load(&path).unwrap().unwrap();
        assert_eq!(loaded.find("claude-live-1").unwrap().id, "claude-1");
        let seat = loaded.find("client").unwrap();
        assert!(seat.live_only);
        assert_eq!(seat.id, "claude-live-2");
    }

    #[test]
    fn a_live_file_without_config_json_is_still_loaded() {
        let temp = tempfile::tempdir().unwrap();
        let (path, registry) = saved_with_a_live_seat(temp.path());
        // A crash on the very first save: the live file landed, config.json
        // did not.
        std::fs::remove_file(&path).unwrap();

        let loaded = Registry::load(&path)
            .unwrap()
            .expect("not an empty registry");
        let live: Vec<&Account> = registry.accounts.iter().filter(|a| a.live_only).collect();
        assert_eq!(loaded.accounts.iter().collect::<Vec<_>>(), live);
    }
}
