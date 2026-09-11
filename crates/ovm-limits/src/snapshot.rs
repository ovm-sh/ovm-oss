//! The on-disk contract. Every consumer — `ovm limits show`, a menu bar, a
//! sweep gate deciding whether an account has quota left — reads
//! `limits.json` and nothing else. Poll internals can change; this shape is
//! versioned by `schema` and only grows.

use crate::paths::LimitsDirs;
use crate::registry::{Account, Provider};
use crate::Result;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::Path;
use std::time::{SystemTime, UNIX_EPOCH};

pub const SCHEMA: &str = "ovm-limits/v1";

/// One rolling usage window as the product reports it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Window {
    /// Stable id: `five_hour`, `seven_day`, `spend_limit` for Claude;
    /// `<limitId>.primary` / `<limitId>.secondary` for Codex.
    pub id: String,
    /// Short human label: `5h`, `7d`, `spend`, `weekly`…
    pub label: String,
    pub used_percent: f64,
    /// Unix epoch seconds when the window resets. Absent when the product
    /// did not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// When this window was last seen to reset, scheduled or not. Carried
    /// from poll to poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_reset_at: Option<u64>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct AccountSnapshot {
    /// The account's stable id — the same string that names its home.
    /// `name` is the field an older build wrote, so snapshots taken before
    /// accounts had ids are still readable after their file is carried across.
    #[serde(alias = "name")]
    pub id: String,
    pub provider: Provider,
    /// The account's label when it has one, so a reader of `limits.json` can
    /// show what the person calls it without opening the config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    /// Unix epoch seconds when the product answered.
    pub captured_at: u64,
    pub host: String,
    /// How the numbers were obtained (`statusline`, `app-server`).
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub plan: Option<String>,
    #[serde(default)]
    pub windows: Vec<Window>,
    /// What one poll of this account cost, when the product reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub poll_cost_usd: Option<f64>,
    /// Codex: reset credits the account can spend to clear a window.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_credits_available: Option<u64>,
    /// Set when the poll failed; `windows` is then empty and the previous
    /// snapshot is gone, so the failure is visible rather than papered over
    /// by stale numbers.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Claude: the model the poll's one turn ran on (`claude-haiku-4-5`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Claude: the Claude Code version that answered (`2.1.263`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_version: Option<String>,
    /// Codex: the account the numbers belong to, as the server names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub account_id: Option<String>,
    /// Codex: the account's paid-credit state, when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub credits: Option<Credits>,
    /// Codex: the account has hit its spend control.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spend_control_reached: Option<bool>,
    /// Codex: which limit was hit, when one was (`rateLimitReachedType`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rate_limit_reached_type: Option<String>,
    /// Codex: every reset credit the account holds, spent or not.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reset_credits: Vec<ResetCredit>,
    /// How many polls in a row have failed, this one included. Zero on a
    /// good poll.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub consecutive_failures: u32,
    /// Whether a `failed` event has been raised for the current streak, so
    /// the recovery that ends it is worth a word too.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub failure_alerted: bool,
    /// Everything the product said about its limits, verbatim: Claude's
    /// `rate_limits` object, Codex's whole `account/rateLimits/read` result.
    /// The typed fields above are the parts this tool understands; this is
    /// the part it does not have to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw: Option<Value>,
}

fn is_zero(n: &u32) -> bool {
    *n == 0
}

/// Codex: paid credits on the account (`rateLimits.credits`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credits {
    pub has_credits: bool,
    pub unlimited: bool,
    /// As the server sends it — a decimal string.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub balance: Option<String>,
}

/// Codex: one rate-limit reset credit (`rateLimitResetCredits.credits[]`).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ResetCredit {
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reset_type: Option<String>,
    /// `available`, `used`, `expired`…
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub granted_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl AccountSnapshot {
    /// A snapshot with nothing in it yet: the account, the clock, the host.
    /// Every poll starts from here and fills in what its product reported.
    pub fn empty(provider: Provider, id: &str, label: Option<String>, source: &str) -> Self {
        Self {
            id: id.into(),
            provider,
            label,
            captured_at: now(),
            host: hostname(),
            source: source.into(),
            plan: None,
            windows: Vec::new(),
            poll_cost_usd: None,
            reset_credits_available: None,
            error: None,
            model: None,
            product_version: None,
            account_id: None,
            credits: None,
            spend_control_reached: None,
            rate_limit_reached_type: None,
            reset_credits: Vec::new(),
            consecutive_failures: 0,
            failure_alerted: false,
            raw: None,
        }
    }

    pub fn for_account(account: &Account, source: &str) -> Self {
        Self::empty(account.provider, &account.id, account.label.clone(), source)
    }

    pub fn failed(account: &Account, source: &str, error: String) -> Self {
        Self {
            error: Some(error),
            ..Self::for_account(account, source)
        }
    }

    /// The same shape the config prints: `claude-1`, or `claude-1 (work)`.
    pub fn display(&self) -> String {
        match &self.label {
            Some(label) => format!("{} ({label})", self.id),
            None => self.id.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Merged {
    pub schema: String,
    pub generated_at: u64,
    pub host: String,
    /// The newest capture across accounts.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<u64>,
    /// When the background poller will next do anything, if it is installed.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_poll_at: Option<u64>,
    /// How often a Claude account is polled at most.
    #[serde(default = "crate::registry::default_interval_minutes")]
    pub interval_minutes: u64,
    pub accounts: Vec<AccountSnapshot>,
    /// The most recent events, oldest first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub events: Vec<crate::events::Event>,
}

/// How many events ride along in the merged view.
const RECENT_EVENTS: usize = 50;

/// Test hook: a fixed clock, so `poll --due` and the table can be exercised
/// against known timestamps. Never set in normal use.
pub const FAKE_NOW_ENV: &str = "OVM_LIMITS_FAKE_NOW";

pub fn now() -> u64 {
    if let Some(fixed) = std::env::var(FAKE_NOW_ENV)
        .ok()
        .and_then(|value| value.parse::<u64>().ok())
    {
        return fixed;
    }
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is valid for its declared length; gethostname
    // NUL-terminates when the name fits.
    let rc = unsafe { libc::gethostname(buffer.as_mut_ptr() as *mut libc::c_char, buffer.len()) };
    if rc != 0 {
        return "unknown".into();
    }
    let end = buffer.iter().position(|&b| b == 0).unwrap_or(buffer.len());
    String::from_utf8_lossy(&buffer[..end]).into_owned()
}

/// Write via a sibling temp file and rename, so a reader never sees a torn
/// file — the merged view is read by other processes on their own clock.
pub fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let file_name = path
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "file".into());
    let temp = path.with_file_name(format!(".{file_name}.{}.tmp", std::process::id()));
    std::fs::write(&temp, bytes)?;
    std::fs::rename(&temp, path)?;
    Ok(())
}

pub fn write_snapshot(dirs: &LimitsDirs, snapshot: &AccountSnapshot) -> Result<()> {
    let json = serde_json::to_string_pretty(snapshot)?;
    write_atomic(&dirs.snapshot_file(&snapshot.id), json.as_bytes())
}

pub fn load_snapshots(dirs: &LimitsDirs) -> Result<Vec<AccountSnapshot>> {
    let mut snapshots = Vec::new();
    let entries = match std::fs::read_dir(dirs.snapshots_dir()) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(snapshots),
        Err(error) => return Err(error.into()),
    };
    for entry in entries {
        let path = entry?.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let raw = std::fs::read_to_string(&path)?;
        match serde_json::from_str::<AccountSnapshot>(&raw) {
            Ok(snapshot) => snapshots.push(snapshot),
            Err(error) => eprintln!(
                "  warning: skipping unreadable snapshot {} ({error})",
                path.display()
            ),
        }
    }
    snapshots.sort_by_key(|a| a.display());
    Ok(snapshots)
}

/// Rebuild `limits.json` from the per-account snapshots, and the public
/// `resets.json` beside it.
pub fn merge(dirs: &LimitsDirs) -> Result<Merged> {
    let registry = crate::registry::Registry::load_or_default(&dirs.config_file())?;
    let accounts = load_snapshots(dirs)?;
    let at = now();
    let last_poll_at = accounts.iter().map(|a| a.captured_at).max();
    let next_poll_at = if crate::agent::is_installed() {
        crate::due::next_poll_at(&registry, &accounts, crate::agent::TICK_SECONDS)
    } else {
        None
    };
    let events = crate::events::recent(dirs, RECENT_EVENTS)?;
    let merged = Merged {
        schema: SCHEMA.into(),
        generated_at: at,
        host: hostname(),
        last_poll_at,
        next_poll_at,
        interval_minutes: registry.interval_minutes,
        accounts,
        events,
    };
    let json = serde_json::to_string_pretty(&merged)?;
    write_atomic(&dirs.merged_file(), json.as_bytes())?;
    let feed = crate::public::feed(&merged, &merged.events);
    let json = serde_json::to_string_pretty(&feed)?;
    write_atomic(&dirs.public_file(), json.as_bytes())?;
    Ok(merged)
}

pub fn load_merged(dirs: &LimitsDirs) -> Result<Option<Merged>> {
    match std::fs::read_to_string(dirs.merged_file()) {
        Ok(raw) => Ok(Some(serde_json::from_str(&raw)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn snapshot(provider: Provider, name: &str) -> AccountSnapshot {
        AccountSnapshot {
            captured_at: 1_700_000_000,
            host: "test".into(),
            windows: vec![Window {
                id: "five_hour".into(),
                label: "5h".into(),
                used_percent: 19.0,
                resets_at: Some(1_700_003_600),
                window_minutes: Some(300),
                last_reset_at: None,
            }],
            ..AccountSnapshot::empty(provider, name, None, "test")
        }
    }

    #[test]
    fn merge_collects_every_snapshot_sorted_and_stamps_the_schema() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        write_snapshot(&dirs, &snapshot(Provider::Codex, "codex-1")).unwrap();
        write_snapshot(&dirs, &snapshot(Provider::Claude, "claude-2")).unwrap();
        write_snapshot(&dirs, &snapshot(Provider::Claude, "claude-1")).unwrap();

        let merged = merge(&dirs).unwrap();
        assert_eq!(merged.schema, SCHEMA);
        let labels: Vec<String> = merged.accounts.iter().map(|a| a.display()).collect();
        assert_eq!(labels, ["claude-1", "claude-2", "codex-1"]);

        let reloaded = load_merged(&dirs).unwrap().expect("merged file written");
        assert_eq!(reloaded, merged);
        assert_eq!(merged.last_poll_at, Some(1_700_000_000));
        assert!(merged.next_poll_at.is_none(), "no agent in a test sandbox");
        let feed: crate::public::Feed =
            serde_json::from_str(&std::fs::read_to_string(dirs.public_file()).unwrap()).unwrap();
        assert_eq!(feed.schema, crate::public::SCHEMA);
        assert!(feed.resets.is_empty());
    }

    #[test]
    fn a_rewritten_snapshot_replaces_the_previous_one() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        write_snapshot(&dirs, &snapshot(Provider::Claude, "default")).unwrap();
        let mut newer = snapshot(Provider::Claude, "default");
        newer.windows[0].used_percent = 42.0;
        write_snapshot(&dirs, &newer).unwrap();
        let merged = merge(&dirs).unwrap();
        assert_eq!(merged.accounts.len(), 1);
        assert_eq!(merged.accounts[0].windows[0].used_percent, 42.0);
    }

    #[test]
    fn write_atomic_leaves_no_temp_file_behind() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested").join("out.json");
        write_atomic(&path, b"{}").unwrap();
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "{}");
        let leftovers: Vec<PathBuf> = std::fs::read_dir(path.parent().unwrap())
            .unwrap()
            .map(|e| e.unwrap().path())
            .filter(|p| p != &path)
            .collect();
        assert!(leftovers.is_empty(), "{leftovers:?}");
    }

    #[test]
    fn failed_snapshot_carries_the_error_and_no_windows() {
        let account = Account {
            id: "default".into(),
            provider: Provider::Claude,
            label: None,
            home: None,
            model: None,
        };
        let failed = AccountSnapshot::failed(&account, "statusline", "boom".into());
        assert_eq!(failed.error.as_deref(), Some("boom"));
        assert!(failed.windows.is_empty());
        let json = serde_json::to_value(&failed).unwrap();
        assert!(json.get("plan").is_none(), "absent optionals stay absent");
        assert!(
            json.get("reset_credits").is_none(),
            "an empty list is left out too"
        );
        assert!(json.get("raw").is_none());
    }

    /// The file is read by other programs on their own schedule, so a
    /// snapshot written before these fields existed must still load.
    #[test]
    fn a_snapshot_from_before_the_extra_fields_still_loads() {
        let older = r#"{"id":"claude-1","provider":"claude","captured_at":1,"host":"h","source":"statusline","windows":[]}"#;
        let loaded: AccountSnapshot = serde_json::from_str(older).unwrap();
        assert!(loaded.raw.is_none());
        assert!(loaded.reset_credits.is_empty());
        assert!(loaded.credits.is_none());
    }
}
