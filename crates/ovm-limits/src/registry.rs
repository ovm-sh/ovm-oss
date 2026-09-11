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
}

impl Account {
    pub fn new(provider: Provider, id: impl Into<String>, label: Option<String>) -> Self {
        Self {
            id: id.into(),
            provider,
            label,
            home: None,
            model: None,
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

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default)]
    pub accounts: Vec<Account>,
    /// How often a Claude account is worth a paid poll. The background agent
    /// ticks far more often than this and only spends a turn once the interval
    /// has passed or a window has reset since the last snapshot.
    #[serde(default = "default_interval_minutes")]
    pub interval_minutes: u64,
    /// Commands to run when something happens; see `hooks`.
    #[serde(default, skip_serializing_if = "Hooks::is_empty")]
    pub hooks: Hooks,
}

impl Default for Registry {
    fn default() -> Self {
        Self {
            accounts: Vec::new(),
            interval_minutes: DEFAULT_INTERVAL_MINUTES,
            hooks: Hooks::default(),
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

impl Registry {
    /// Bounds on what the agent may be told to do. Below five minutes every
    /// tick would pay; above a week the numbers stop meaning anything.
    pub const MIN_INTERVAL_MINUTES: u64 = 5;
    pub const MAX_INTERVAL_MINUTES: u64 = 7 * 24 * 60;

    /// `None` when no account has ever been added — the file is the switch.
    pub fn load(path: &Path) -> Result<Option<Self>> {
        let raw = match std::fs::read_to_string(path) {
            Ok(raw) if raw.trim().is_empty() => return Ok(None),
            Ok(raw) => raw,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
        };
        let registry: Self = serde_json::from_str(&raw).map_err(|error| {
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
        registry
            .validate()
            .map_err(|error| LimitsError::Message(format!("{}: {error}", path.display())))?;
        Ok(Some(registry))
    }

    /// Whatever is on disk, or an empty registry about to be written.
    pub fn load_or_default(path: &Path) -> Result<Self> {
        Ok(Self::load(path)?.unwrap_or_default())
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let json = serde_json::to_string_pretty(self)?;
        crate::snapshot::write_atomic(path, json.as_bytes())
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
        (1..)
            .map(|n| format!("{provider}-{n}"))
            .find(|candidate| {
                !self
                    .accounts
                    .iter()
                    .any(|a| a.id.eq_ignore_ascii_case(candidate))
            })
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
            })
            .unwrap();
        registry.save(&path).unwrap();
        assert_eq!(Registry::load(&path).unwrap().unwrap(), registry);
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
}
