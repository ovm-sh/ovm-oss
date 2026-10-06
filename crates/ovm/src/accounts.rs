//! Claude accounts that run side by side, sharing one brain.
//!
//! An account is a folder, `~/.claude-accounts/<label>/`, used as Claude Code's
//! `CLAUDE_CONFIG_DIR`. Claude Code keys its login (the macOS Keychain item and
//! `.claude.json`'s `oauthAccount`) to that folder, so each account is its own
//! login and signing one in never touches another — least of all `~/.claude`,
//! which stays the default login and is never logged in or out from here.
//!
//! Everything that is the person rather than the login is shared: a fixed
//! allowlist of `~/.claude` entries is symlinked in (instructions, memory,
//! skills, plugins, agents, commands, hooks, settings, and `projects/` — so
//! every account sees every transcript and a session can be resumed under
//! another account). Anything not on the list stays per account; in
//! particular `policy-limits.json`, which is org policy and differs between a
//! personal plan and a team seat.
//!
//! `.claude.json` is not shared — it carries the login. Each folder gets its
//! own, seeded with the MCP servers and project trust of the main one and
//! nothing else, and refreshed on every launch so a server added to the main
//! setup reaches every account.
//!
//! What kind of account it is (personal plan or team seat, which org, which
//! seat tier) is read from the login itself, not typed in.

use crate::error::{OvmError, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};

/// `~/.claude` entries every account shares. An allowlist on purpose: a new
/// file Claude Code starts writing stays per account until someone decides it
/// is safe to share.
pub const SHARED: &[&str] = &[
    "CLAUDE.md",
    "agents",
    "commands",
    "hooks",
    "skills",
    "plugins",
    "output-styles",
    "settings.json",
    "keybindings.json",
    "projects",
    "plans",
    "todos",
    "history.jsonl",
    "file-history",
];

/// `.claude.json` keys a folder takes from the main one. Everything else —
/// above all `oauthAccount` and `userID` — is the folder's own.
const SEEDED_KEYS: &[&str] = &["mcpServers", "projects"];

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Entry {
    /// `team` or `personal` when the person overrides what the login says.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    /// A paid API provider instead of a subscription login. When set, the
    /// account folder still exists (shared brain, separate policy), but launch
    /// injects the provider's environment variables instead of relying on an
    /// OAuth login.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider: Option<ApiProvider>,
}

/// A paid API backend that Claude Code can target instead of the subscription.
/// Each variant carries only the connection details; billing is the cloud
/// provider's concern, and ovm shows a loud notice at launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ApiProvider {
    /// Azure AI Foundry (formerly Azure OpenAI Service).
    Azure {
        /// `https://<resource>.cognitiveservices.azure.com`
        endpoint: String,
        /// The API key. Stored in accounts.json, which is 0600.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        api_key: Option<String>,
    },
    /// AWS Bedrock.
    Bedrock {
        /// AWS region, e.g. `us-east-1`.
        region: String,
        /// AWS CLI profile name for SSO / credentials.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        profile: Option<String>,
    },
}

impl ApiProvider {
    /// The display name for the launch narration.
    pub fn display_name(&self) -> &str {
        match self {
            Self::Azure { .. } => "Azure",
            Self::Bedrock { .. } => "Bedrock",
        }
    }

    /// Set the environment variables Claude Code reads for this provider.
    /// # Safety
    /// Only at launch, before any threads.
    pub fn set_env(&self) {
        match self {
            Self::Azure { endpoint, api_key } => {
                std::env::set_var("CLAUDE_CODE_USE_AZURE", "1");
                std::env::set_var("ANTHROPIC_BASE_URL", endpoint);
                if let Some(key) = api_key {
                    std::env::set_var("ANTHROPIC_API_KEY", key);
                }
            }
            Self::Bedrock { region, profile } => {
                std::env::set_var("CLAUDE_CODE_USE_BEDROCK", "1");
                std::env::set_var("AWS_REGION", region);
                if let Some(profile) = profile {
                    std::env::set_var("AWS_PROFILE", profile);
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Binding {
    /// An absolute directory; a launch at or under it picks `account`.
    pub dir: PathBuf,
    pub account: String,
}

/// Failover: an ordered chain of accounts; at launch, pick the first one
/// whose highest usage window is below the threshold. Never mid-session.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Failover {
    /// Ordered account labels: try the first, fall through to the next.
    pub chain: Vec<String>,
    /// The usage percentage above which an account is considered spent.
    /// Default 95.
    #[serde(default = "default_threshold")]
    pub threshold: f64,
}

pub const DEFAULT_THRESHOLD: f64 = 95.0;

fn default_threshold() -> f64 {
    DEFAULT_THRESHOLD
}

impl Default for Failover {
    fn default() -> Self {
        Self {
            chain: Vec::new(),
            threshold: default_threshold(),
        }
    }
}

impl Failover {
    pub fn is_empty(&self) -> bool {
        self.chain.is_empty()
    }
}

impl Eq for Failover {}

fn failover_is_empty(f: &Failover) -> bool {
    f.is_empty()
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Accounts {
    #[serde(default)]
    pub accounts: BTreeMap<String, Entry>,
    #[serde(default)]
    pub bindings: Vec<Binding>,
    /// The account a launch uses outside every binding. None = `~/.claude`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default: Option<String>,
    /// When set, a launch walks the chain and picks the first account under
    /// the threshold. The chain replaces the default: the first entry is the
    /// preferred account, and the rest are fallbacks.
    #[serde(default, skip_serializing_if = "failover_is_empty")]
    pub failover: Failover,
}

/// Where everything lives. Injected so tests never touch a real home.
#[derive(Debug, Clone)]
pub struct Layout {
    /// `~/.ovm/accounts.json`.
    pub file: PathBuf,
    /// `~/.claude-accounts`.
    pub folders: PathBuf,
    /// `~/.claude` — the shared brain and the default login.
    pub main_dir: PathBuf,
    /// `~/.claude.json` — the default login's state, beside `~/.claude`.
    pub main_json: PathBuf,
}

impl Layout {
    pub fn new(ovm_base: &Path) -> Result<Self> {
        let home = dirs::home_dir().ok_or_else(|| OvmError::Message("no home directory".into()))?;
        Ok(Self {
            file: ovm_base.join("accounts.json"),
            folders: home.join(".claude-accounts"),
            main_dir: home.join(".claude"),
            main_json: home.join(".claude.json"),
        })
    }

    pub fn folder(&self, label: &str) -> PathBuf {
        self.folders.join(label)
    }
}

impl Accounts {
    pub fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => serde_json::from_str(&raw).map_err(|error| {
                OvmError::Message(format!("{} is not valid: {error}", path.display()))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error.into()),
        }
    }

    pub fn save(&self, path: &Path) -> Result<()> {
        let text = serde_json::to_string_pretty(self)? + "\n";
        write_atomic(path, text.as_bytes())
    }

    /// The account a launch in `cwd` uses, and why: the deepest binding that
    /// contains `cwd`, else the default. None means `~/.claude`.
    /// Used by `apply_binding` tests and as the non-failover fallback.
    #[allow(dead_code)]
    pub fn resolve(&self, cwd: &Path) -> Option<(String, String)> {
        self.bound(cwd).or_else(|| {
            self.default
                .clone()
                .map(|label| (label, "the default account".to_string()))
        })
    }

    /// Like [`resolve`], but when failover is configured and no binding
    /// matches, walk the chain instead of using the default. The first
    /// account whose highest usage window is below the threshold wins; a
    /// provider (API) account has no usage limits and always qualifies.
    /// Falls back to the chain's first account when limits data is
    /// unavailable or every account is spent. `limits_dir` holds ovm-limits'
    /// `limits.json` and `config.json`, both read only; a snapshot whose
    /// newest chain reading is older than twice the poll interval at `now`
    /// (unix seconds) counts as no data.
    pub fn resolve_with_failover(&self, cwd: &Path, limits_dir: &Path, now: u64) -> Option<Pick> {
        // A binding still wins unconditionally.
        if let Some((label, why)) = self.bound(cwd) {
            return Some(Pick::plain(label, why));
        }
        // No chain → plain default.
        if self.failover.is_empty() {
            return self.default_pick();
        }
        let usage = read_limits_usage(&limits_dir.join("limits.json"));
        let interval = read_poll_interval_minutes(&limits_dir.join("config.json"));
        let snapshot = self.judge_snapshot(usage, interval, now);
        self.walk_chain(&snapshot)
    }

    /// Is the snapshot fresh enough to decide on? Its age is that of the
    /// newest reading among the chain's accounts; a reading without a
    /// `captured_at` cannot be aged and is trusted.
    fn judge_snapshot(
        &self,
        usage: Option<HashMap<String, Usage>>,
        interval_minutes: u64,
        now: u64,
    ) -> Snapshot {
        let Some(usage) = usage else {
            return Snapshot::Missing;
        };
        let newest = self
            .failover
            .chain
            .iter()
            .filter_map(|label| usage.get(label.as_str()))
            .filter_map(|u| u.captured_at)
            .max();
        if let Some(newest) = newest {
            let age = now.saturating_sub(newest);
            let limit = STALE_AFTER_INTERVALS * interval_minutes * SECONDS_PER_MINUTE;
            if age > limit {
                return Snapshot::Stale {
                    minutes: age / SECONDS_PER_MINUTE,
                };
            }
        }
        Snapshot::Fresh(usage)
    }

    /// The default account as a pick, if there is one.
    fn default_pick(&self) -> Option<Pick> {
        self.default
            .clone()
            .map(|label| Pick::plain(label, "the default account".to_string()))
    }

    /// The chain walk itself, on usage already read.
    fn walk_chain(&self, snapshot: &Snapshot) -> Option<Pick> {
        let threshold = self.failover.threshold;
        let (usage, unknown) = match snapshot {
            Snapshot::Fresh(usage) => (Some(usage), "no usage data, first available".to_string()),
            Snapshot::Missing => (None, "no usage data, first available".to_string()),
            Snapshot::Stale { minutes } => (
                None,
                format!("usage data {minutes} min old, treating as unknown; first available"),
            ),
        };
        let mut skipped: Vec<Skip> = Vec::new();
        for label in &self.failover.chain {
            let Some(entry) = self.accounts.get(label) else {
                continue;
            };
            let reason = if entry.provider.is_some() {
                "API account, no usage limits".to_string()
            } else {
                match usage.and_then(|u| u.get(label.as_str())) {
                    Some(u) if u.percent >= threshold => {
                        skipped.push(Skip {
                            label: label.clone(),
                            reading: format!("{} {:.0}% ≥ {threshold:.0}%", u.window, u.percent),
                        });
                        continue;
                    }
                    Some(u) => format!("{label} {} {:.0}% < {threshold:.0}%", u.window, u.percent),
                    None => unknown.clone(),
                }
            };
            let mut parts: Vec<String> = skipped
                .iter()
                .map(|skip| format!("{} {}, skipped", skip.label, skip.reading))
                .collect();
            parts.push(reason);
            return Some(Pick {
                label: label.clone(),
                why: format!("failover chain ({})", parts.join("; ")),
                skipped,
            });
        }
        // Every account in the chain is above the threshold — use the first
        // one that is an account anyway (better to launch than to refuse). A
        // chain of non-accounts falls through to the default.
        let Some(first) = self
            .failover
            .chain
            .iter()
            .find(|label| self.accounts.contains_key(label.as_str()))
        else {
            return self.default_pick();
        };
        Some(Pick {
            label: first.clone(),
            why: format!(
                "failover chain (all accounts above {threshold:.0}% threshold, using first)"
            ),
            skipped,
        })
    }

    /// The deepest binding at or above `cwd`, and why — without the default.
    pub fn bound(&self, cwd: &Path) -> Option<(String, String)> {
        self.bindings
            .iter()
            .filter(|binding| cwd.starts_with(&binding.dir))
            .max_by_key(|binding| binding.dir.components().count())
            .map(|binding| {
                (
                    binding.account.clone(),
                    format!("bound to {}", binding.dir.display()),
                )
            })
    }
}

/// What a launch resolved to: the account, why, and the chain accounts the
/// walk passed over for usage on the way there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pick {
    pub label: String,
    pub why: String,
    pub skipped: Vec<Skip>,
}

impl Pick {
    fn plain(label: String, why: String) -> Self {
        Self {
            label,
            why,
            skipped: Vec::new(),
        }
    }
}

/// A chain account passed over because its highest window was at or above
/// the threshold. `reading` names that window: `5h 96% ≥ 95%`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Skip {
    pub label: String,
    pub reading: String,
}

/// One account's highest window in the limits snapshot.
#[derive(Debug, Clone, PartialEq)]
struct Usage {
    percent: f64,
    /// The window's short label (`5h`, `7d`), else its id.
    window: String,
    /// When the reading was taken, unix seconds.
    captured_at: Option<u64>,
}

/// The limits snapshot as the chain walk sees it.
#[derive(Debug, Clone, PartialEq)]
enum Snapshot {
    /// No `limits.json`, or one that does not parse.
    Missing,
    /// Too old to decide on: every account counts as unknown.
    Stale {
        minutes: u64,
    },
    Fresh(HashMap<String, Usage>),
}

/// ovm-limits' poll interval when its `config.json` does not say. Mirrors
/// `DEFAULT_INTERVAL_MINUTES` in `crates/ovm-limits/src/registry.rs` (that
/// crate is a binary, so it cannot be imported); a test fails if they drift.
const DEFAULT_POLL_INTERVAL_MINUTES: u64 = 60;
/// A snapshot older than this many poll intervals is treated as unknown.
const STALE_AFTER_INTERVALS: u64 = 2;
const SECONDS_PER_MINUTE: u64 = 60;

/// The poll interval from ovm-limits' `config.json` (`interval_minutes`),
/// read only. Missing, unreadable or zero means the default.
fn read_poll_interval_minutes(path: &Path) -> u64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        .and_then(|doc| doc.get("interval_minutes").and_then(Value::as_u64))
        .filter(|minutes| *minutes > 0)
        .unwrap_or(DEFAULT_POLL_INTERVAL_MINUTES)
}

/// Read `limits.json` and return each account label's highest window.
/// Returns `None` when the file is missing or unreadable — callers fall back
/// gracefully.
fn read_limits_usage(path: &Path) -> Option<HashMap<String, Usage>> {
    let raw = std::fs::read_to_string(path).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    let accounts = doc.get("accounts")?.as_array()?;
    let mut usage = HashMap::new();
    for account in accounts {
        // The label is what accounts.json uses; the id is what ovm-limits uses.
        // Check both: the account may or may not have a label. An entry with
        // neither, or without windows, is skipped; the rest still count.
        let Some(label) = account
            .get("label")
            .and_then(Value::as_str)
            .or_else(|| account.get("id").and_then(Value::as_str))
        else {
            continue;
        };
        let Some(windows) = account.get("windows").and_then(Value::as_array) else {
            continue;
        };
        let mut highest: Option<Usage> = None;
        for window in windows {
            let Some(percent) = window.get("used_percent").and_then(Value::as_f64) else {
                continue;
            };
            if highest.as_ref().is_some_and(|h| h.percent >= percent) {
                continue;
            }
            let name = window
                .get("label")
                .and_then(Value::as_str)
                .or_else(|| window.get("id").and_then(Value::as_str))
                .unwrap_or("window");
            highest = Some(Usage {
                percent,
                window: name.to_string(),
                captured_at: None,
            });
        }
        if let Some(mut highest) = highest {
            highest.captured_at = account.get("captured_at").and_then(Value::as_u64);
            usage.insert(label.to_string(), highest);
        }
    }
    Some(usage)
}

/// Labels become folder names and must stay boring.
pub fn validate_label(label: &str) -> Result<()> {
    let ok = !label.is_empty()
        && label.len() <= 40
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
        && !label.starts_with('-');
    if !ok {
        return Err(OvmError::Message(format!(
            "`{label}` is not an account label — use letters, digits, - and _"
        )));
    }
    Ok(())
}

/// What `prepare` did, for the person to read.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Prepared {
    pub linked: Vec<String>,
    /// Entries left alone because the folder already has its own.
    pub own: Vec<String>,
}

/// Make `folder` an account folder: create it, link the shared brain, and seed
/// or refresh its `.claude.json`. Idempotent; run on every launch.
pub fn prepare(layout: &Layout, folder: &Path) -> Result<Prepared> {
    if same_path(folder, &layout.main_dir) {
        return Err(OvmError::Message(
            "refusing to use ~/.claude as an account folder: it is the default login".into(),
        ));
    }
    std::fs::create_dir_all(folder)?;
    let mut prepared = Prepared::default();
    for name in SHARED {
        let target = layout.main_dir.join(name);
        if !target.exists() {
            continue;
        }
        let link = folder.join(name);
        match std::fs::symlink_metadata(&link) {
            Err(_) => {
                std::os::unix::fs::symlink(&target, &link)?;
                prepared.linked.push((*name).to_string());
            }
            Ok(meta) if meta.file_type().is_symlink() => {
                if std::fs::read_link(&link).ok().as_deref() != Some(target.as_path()) {
                    prepared.own.push((*name).to_string());
                }
            }
            Ok(_) => prepared.own.push((*name).to_string()),
        }
    }
    seed_claude_json(&layout.main_json, &folder.join(".claude.json"))?;
    Ok(prepared)
}

/// Take `SEEDED_KEYS` from the main `.claude.json` into the folder's, keeping
/// everything the folder already has — its login above all. MCP servers are
/// refreshed from the main file; project entries are added only where the
/// folder has none, so what an account learned about a project is kept.
fn seed_claude_json(main_json: &Path, folder_json: &Path) -> Result<()> {
    let main: Value = std::fs::read_to_string(main_json)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .unwrap_or_else(|| serde_json::json!({}));
    let mut own: Value = std::fs::read_to_string(folder_json)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
        .filter(Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    let before = own.clone();
    let own_map = own.as_object_mut().expect("checked object");
    own_map
        .entry("hasCompletedOnboarding")
        .or_insert(Value::Bool(true));
    for key in SEEDED_KEYS {
        let Some(Value::Object(from_main)) = main.get(*key) else {
            continue;
        };
        let slot = own_map
            .entry((*key).to_string())
            .or_insert_with(|| Value::Object(Default::default()));
        let Some(slot) = slot.as_object_mut() else {
            continue;
        };
        for (name, value) in from_main {
            if *key == "mcpServers" || !slot.contains_key(name) {
                slot.insert(name.clone(), value.clone());
            }
        }
    }
    if own != before || !folder_json.exists() {
        let text = serde_json::to_string_pretty(&own)? + "\n";
        write_atomic(folder_json, text.as_bytes())?;
    }
    Ok(())
}

/// Who a folder is signed in as, from its `.claude.json` — never the Keychain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Login {
    pub account_uuid: String,
    pub email: Option<String>,
    pub org_uuid: Option<String>,
    pub org_name: Option<String>,
    /// `claude_max`, `claude_pro`, `claude_team`, `claude_enterprise`, …
    pub org_type: Option<String>,
    pub seat_tier: Option<String>,
    pub rate_limit_tier: Option<String>,
}

pub fn login(claude_json: &Path) -> Option<Login> {
    let doc: Value = serde_json::from_str(&std::fs::read_to_string(claude_json).ok()?).ok()?;
    let account = doc.get("oauthAccount")?;
    let text = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    Some(Login {
        account_uuid: text("accountUuid")?,
        email: text("emailAddress"),
        org_uuid: text("organizationUuid"),
        org_name: text("organizationName"),
        org_type: text("organizationType"),
        seat_tier: text("seatTier"),
        rate_limit_tier: text("organizationRateLimitTier").or_else(|| text("userRateLimitTier")),
    })
}

/// `team` for a seat in an organisation's plan, `personal` for an own plan —
/// from the override when there is one, else from what the login reports.
pub fn kind(login: Option<&Login>, entry: &Entry) -> String {
    if let Some(kind) = &entry.kind {
        return kind.clone();
    }
    let team = login.is_some_and(|login| {
        login.seat_tier.is_some()
            || login
                .org_type
                .as_deref()
                .is_some_and(|t| t.contains("team") || t.contains("enterprise"))
    });
    if team {
        "team".into()
    } else {
        "personal".into()
    }
}

fn same_path(a: &Path, b: &Path) -> bool {
    let a = a.canonicalize().unwrap_or_else(|_| a.to_path_buf());
    let b = b.canonicalize().unwrap_or_else(|_| b.to_path_buf());
    a == b
}

fn write_atomic(path: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write;
    use std::os::unix::fs::OpenOptionsExt;
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent)?;
    let tmp = parent.join(format!(
        ".{}.{}.tmp",
        path.file_name().and_then(|n| n.to_str()).unwrap_or("file"),
        std::process::id()
    ));
    {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn layout(root: &Path) -> Layout {
        let main_dir = root.join(".claude");
        std::fs::create_dir_all(main_dir.join("projects")).unwrap();
        std::fs::create_dir_all(main_dir.join("skills")).unwrap();
        std::fs::write(main_dir.join("CLAUDE.md"), "# me\n").unwrap();
        std::fs::write(main_dir.join("settings.json"), "{}").unwrap();
        std::fs::write(
            main_dir.join("policy-limits.json"),
            "{\"org\":\"personal\"}",
        )
        .unwrap();
        std::fs::write(
            root.join(".claude.json"),
            serde_json::json!({
                "oauthAccount": {"accountUuid": "main-login"},
                "userID": "main-user",
                "mcpServers": {"notes": {"command": "notes-mcp"}},
                "projects": {"/work/app": {"hasTrustDialogAccepted": true}}
            })
            .to_string(),
        )
        .unwrap();
        Layout {
            file: root.join("ovm/accounts.json"),
            folders: root.join(".claude-accounts"),
            main_dir,
            main_json: root.join(".claude.json"),
        }
    }

    #[test]
    fn a_folder_shares_the_brain_but_never_the_login_or_org_policy() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout(temp.path());
        let folder = layout.folder("simcity");
        let prepared = prepare(&layout, &folder).unwrap();
        for shared in ["CLAUDE.md", "projects", "skills", "settings.json"] {
            let link = folder.join(shared);
            assert!(
                std::fs::symlink_metadata(&link)
                    .unwrap()
                    .file_type()
                    .is_symlink(),
                "{shared}"
            );
            assert_eq!(
                std::fs::read_link(&link).unwrap(),
                layout.main_dir.join(shared)
            );
            assert!(prepared.linked.contains(&shared.to_string()));
        }
        assert!(
            !folder.join("policy-limits.json").exists(),
            "org policy stays per account"
        );
        let own: Value =
            serde_json::from_str(&std::fs::read_to_string(folder.join(".claude.json")).unwrap())
                .unwrap();
        assert!(own.get("oauthAccount").is_none(), "{own}");
        assert!(own.get("userID").is_none(), "{own}");
        assert_eq!(own["mcpServers"]["notes"]["command"], "notes-mcp");
        assert_eq!(own["projects"]["/work/app"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn preparing_again_keeps_the_folders_login_and_refreshes_mcp_servers() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout(temp.path());
        let folder = layout.folder("team-a");
        prepare(&layout, &folder).unwrap();
        // The person signs in inside the folder; Claude Code writes its login.
        let json_path = folder.join(".claude.json");
        let mut own: Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        own["oauthAccount"] =
            serde_json::json!({"accountUuid": "team-login", "seatTier": "standard"});
        own["projects"]["/work/app"]["allowedTools"] = serde_json::json!(["Bash"]);
        std::fs::write(&json_path, own.to_string()).unwrap();
        // A server is added to the main setup afterwards.
        let mut main: Value =
            serde_json::from_str(&std::fs::read_to_string(&layout.main_json).unwrap()).unwrap();
        main["mcpServers"]["search"] = serde_json::json!({"command": "search-mcp"});
        std::fs::write(&layout.main_json, main.to_string()).unwrap();

        prepare(&layout, &folder).unwrap();
        let own: Value =
            serde_json::from_str(&std::fs::read_to_string(&json_path).unwrap()).unwrap();
        assert_eq!(own["oauthAccount"]["accountUuid"], "team-login");
        assert_eq!(own["mcpServers"]["search"]["command"], "search-mcp");
        assert_eq!(own["projects"]["/work/app"]["allowedTools"][0], "Bash");
        let main_after: Value =
            serde_json::from_str(&std::fs::read_to_string(&layout.main_json).unwrap()).unwrap();
        assert_eq!(
            main_after["oauthAccount"]["accountUuid"], "main-login",
            "main login untouched"
        );
    }

    #[test]
    fn the_main_home_is_never_an_account_folder() {
        let temp = tempfile::tempdir().unwrap();
        let layout = layout(temp.path());
        let error = prepare(&layout, &layout.main_dir.clone())
            .unwrap_err()
            .to_string();
        assert!(error.contains("default login"), "{error}");
    }

    #[test]
    fn the_deepest_binding_wins_and_the_default_covers_the_rest() {
        let accounts = Accounts {
            bindings: vec![
                Binding {
                    dir: "/work".into(),
                    account: "team-a".into(),
                },
                Binding {
                    dir: "/work/client-b".into(),
                    account: "team-b".into(),
                },
            ],
            default: Some("simcity".into()),
            ..Accounts::default()
        };
        assert_eq!(
            accounts.resolve(Path::new("/work/client-b/api")).unwrap().0,
            "team-b"
        );
        assert_eq!(
            accounts.resolve(Path::new("/work/other")).unwrap().0,
            "team-a"
        );
        assert_eq!(
            accounts.resolve(Path::new("/home/me")).unwrap().0,
            "simcity"
        );
        // `/workshop` is not under `/work`.
        assert_eq!(
            accounts.resolve(Path::new("/workshop")).unwrap().0,
            "simcity"
        );
        assert!(Accounts::default().resolve(Path::new("/work")).is_none());
    }

    #[test]
    fn the_kind_is_read_from_the_login() {
        let max = Login {
            org_type: Some("claude_max".into()),
            ..Login::default()
        };
        let team = Login {
            org_type: Some("claude_team".into()),
            ..Login::default()
        };
        let seat = Login {
            seat_tier: Some("standard".into()),
            ..Login::default()
        };
        assert_eq!(kind(Some(&max), &Entry::default()), "personal");
        assert_eq!(kind(Some(&team), &Entry::default()), "team");
        assert_eq!(kind(Some(&seat), &Entry::default()), "team");
        assert_eq!(
            kind(
                None,
                &Entry {
                    kind: Some("team".into()),
                    provider: None,
                }
            ),
            "team"
        );
    }

    #[test]
    fn labels_are_folder_safe() {
        assert!(validate_label("simcity").is_ok());
        assert!(validate_label("team_a-2").is_ok());
        for bad in ["", "../x", "a/b", "-x", "a b"] {
            assert!(validate_label(bad).is_err(), "{bad}");
        }
    }

    fn accounts_with_failover() -> Accounts {
        let mut accounts = BTreeMap::new();
        accounts.insert("simcity".into(), Entry::default());
        accounts.insert("mochi".into(), Entry::default());
        Accounts {
            accounts,
            failover: Failover {
                chain: vec!["simcity".into(), "mochi".into()],
                threshold: 95.0,
            },
            ..Accounts::default()
        }
    }

    /// A moment shortly after the test snapshot's `captured_at` of 1000.
    const FRESH_NOW: u64 = 1000 + 60;

    /// Writes `limits.json` into `dir` and returns `dir`, the limits dir.
    fn write_limits(dir: &Path, data: &[(&str, f64)]) -> PathBuf {
        let limits = dir.join("limits.json");
        let accounts: Vec<Value> = data
            .iter()
            .map(|(label, pct)| {
                serde_json::json!({
                    "id": format!("claude-{label}"),
                    "label": label,
                    "provider": "claude",
                    "captured_at": 1000,
                    "host": "test",
                    "source": "poll",
                    "windows": [{"id": "seven_day", "label": "7d", "used_percent": pct}]
                })
            })
            .collect();
        let doc = serde_json::json!({
            "schema": "ovm-limits/v1",
            "generated_at": 1000,
            "host": "test",
            "accounts": accounts
        });
        std::fs::write(&limits, doc.to_string()).unwrap();
        dir.to_path_buf()
    }

    #[test]
    fn failover_picks_first_account_under_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 97.0), ("mochi", 40.0)]);
        let accounts = accounts_with_failover();
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "mochi");
        assert_eq!(
            why,
            "failover chain (simcity 7d 97% ≥ 95%, skipped; mochi 7d 40% < 95%)"
        );
    }

    #[test]
    fn failover_picks_first_when_under_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 50.0), ("mochi", 40.0)]);
        let accounts = accounts_with_failover();
        let Pick { label, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "simcity");
    }

    #[test]
    fn failover_falls_back_to_first_when_all_above_threshold() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0), ("mochi", 98.0)]);
        let accounts = accounts_with_failover();
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "simcity");
        assert!(why.contains("all accounts above"), "{why}");
    }

    #[test]
    fn failover_works_without_limits_data() {
        let accounts = accounts_with_failover();
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(
                Path::new("/somewhere"),
                Path::new("/nonexistent"),
                FRESH_NOW,
            )
            .unwrap();
        assert_eq!(label, "simcity", "first in chain when no data");
        assert!(why.contains("no usage data"), "{why}");
    }

    #[test]
    fn binding_overrides_failover() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0), ("mochi", 10.0)]);
        let mut accounts = accounts_with_failover();
        accounts.bindings.push(Binding {
            dir: "/work".into(),
            account: "simcity".into(),
        });
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/work/project"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "simcity", "binding wins even when simcity is at 99%");
        assert!(why.contains("bound"), "{why}");
    }

    #[test]
    fn no_failover_chain_falls_back_to_default() {
        let mut accounts = accounts_with_failover();
        accounts.failover.chain.clear();
        accounts.default = Some("mochi".into());
        let Pick { label, .. } = accounts
            .resolve_with_failover(
                Path::new("/somewhere"),
                Path::new("/nonexistent"),
                FRESH_NOW,
            )
            .unwrap();
        assert_eq!(label, "mochi");
    }

    #[test]
    fn the_highest_window_decides_and_is_named() {
        let temp = tempfile::tempdir().unwrap();
        let limits = temp.path().join("limits.json");
        let doc = serde_json::json!({
            "accounts": [
                {"label": "simcity", "windows": [
                    {"id": "five_hour", "label": "5h", "used_percent": 96.0},
                    {"id": "seven_day", "label": "7d", "used_percent": 10.0}
                ]},
                {"label": "mochi", "windows": [
                    {"id": "five_hour", "used_percent": 3.0},
                    {"id": "seven_day", "used_percent": 10.0}
                ]}
            ]
        });
        std::fs::write(&limits, doc.to_string()).unwrap();
        let pick = accounts_with_failover()
            .resolve_with_failover(Path::new("/somewhere"), temp.path(), FRESH_NOW)
            .unwrap();
        assert_eq!(pick.label, "mochi");
        assert_eq!(
            pick.why,
            "failover chain (simcity 5h 96% ≥ 95%, skipped; mochi seven_day 10% < 95%)"
        );
        assert_eq!(
            pick.skipped,
            vec![Skip {
                label: "simcity".into(),
                reading: "5h 96% ≥ 95%".into(),
            }]
        );
    }

    #[test]
    fn a_provider_account_has_no_usage_limits() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0)]);
        let mut accounts = accounts_with_failover();
        accounts.accounts.insert(
            "mochi".into(),
            Entry {
                kind: None,
                provider: Some(ApiProvider::Bedrock {
                    region: "us-east-1".into(),
                    profile: None,
                }),
            },
        );
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "mochi");
        assert_eq!(
            why,
            "failover chain (simcity 7d 99% ≥ 95%, skipped; API account, no usage limits)"
        );
        accounts.failover.chain.reverse();
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "mochi");
        assert_eq!(why, "failover chain (API account, no usage limits)");
    }

    #[test]
    fn all_spent_fallback_skips_chain_labels_that_are_not_accounts() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0), ("mochi", 98.0)]);
        let mut accounts = accounts_with_failover();
        accounts.failover.chain.insert(0, "codex-1".into());
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, FRESH_NOW)
            .unwrap();
        assert_eq!(label, "simcity");
        assert!(why.contains("all accounts above"), "{why}");
    }

    #[test]
    fn a_chain_without_accounts_falls_through_to_the_default() {
        let mut accounts = accounts_with_failover();
        accounts.failover.chain = vec!["codex-1".into(), "codex-2".into()];
        assert_eq!(
            accounts.resolve_with_failover(
                Path::new("/somewhere"),
                Path::new("/nonexistent"),
                FRESH_NOW
            ),
            None
        );
        accounts.default = Some("mochi".into());
        let Pick { label, why, .. } = accounts
            .resolve_with_failover(
                Path::new("/somewhere"),
                Path::new("/nonexistent"),
                FRESH_NOW,
            )
            .unwrap();
        assert_eq!(label, "mochi");
        assert_eq!(why, "the default account");
    }

    #[test]
    fn a_malformed_snapshot_entry_keeps_the_rest() {
        let temp = tempfile::tempdir().unwrap();
        let limits = temp.path().join("limits.json");
        let doc = serde_json::json!({
            "accounts": [
                {"provider": "claude", "windows": []},
                {"id": "claude-x", "label": "simcity"},
                {"label": "simcity", "windows": [{"label": "7d", "used_percent": 97.0}]},
                {"label": "mochi", "windows": [{"label": "7d", "used_percent": 20.0}]}
            ]
        });
        std::fs::write(&limits, doc.to_string()).unwrap();
        let usage = read_limits_usage(&limits).unwrap();
        assert_eq!(usage.len(), 2, "{usage:?}");
        assert_eq!(usage["simcity"].percent, 97.0);
        assert_eq!(usage["mochi"].percent, 20.0);
    }

    #[test]
    fn a_snapshot_older_than_twice_the_poll_interval_is_unknown() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0), ("mochi", 10.0)]);
        let accounts = accounts_with_failover();
        // Default interval 60 min: 120 min old is still trusted, 121 is not.
        let at_limit = 1000 + 120 * SECONDS_PER_MINUTE;
        let Pick { label, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, at_limit)
            .unwrap();
        assert_eq!(label, "mochi");
        let past_limit = 1000 + 121 * SECONDS_PER_MINUTE;
        let Pick {
            label,
            why,
            skipped,
        } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, past_limit)
            .unwrap();
        assert_eq!(label, "simcity");
        assert_eq!(
            why,
            "failover chain (usage data 121 min old, treating as unknown; first available)"
        );
        assert!(skipped.is_empty());
    }

    #[test]
    fn the_stale_rule_honours_the_configured_interval() {
        let temp = tempfile::tempdir().unwrap();
        let limits = write_limits(temp.path(), &[("simcity", 99.0), ("mochi", 10.0)]);
        let accounts = accounts_with_failover();
        let forty_five_minutes = 1000 + 45 * SECONDS_PER_MINUTE;
        let Pick { label, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, forty_five_minutes)
            .unwrap();
        assert_eq!(label, "mochi", "45 min is fresh under the default interval");
        std::fs::write(
            temp.path().join("config.json"),
            r#"{"interval_minutes": 15}"#,
        )
        .unwrap();
        let Pick { label, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), &limits, forty_five_minutes)
            .unwrap();
        assert_eq!(label, "simcity", "45 min is stale under a 15 min interval");
        assert_eq!(
            read_poll_interval_minutes(&temp.path().join("config.json")),
            15
        );
        assert_eq!(
            read_poll_interval_minutes(&temp.path().join("absent.json")),
            DEFAULT_POLL_INTERVAL_MINUTES
        );
    }

    #[test]
    fn staleness_is_judged_on_the_chains_newest_reading() {
        let temp = tempfile::tempdir().unwrap();
        let limits = temp.path().join("limits.json");
        let doc = serde_json::json!({
            "accounts": [
                {"label": "simcity", "captured_at": 1000,
                 "windows": [{"label": "7d", "used_percent": 99.0}]},
                {"label": "mochi", "captured_at": 1000 + 50 * SECONDS_PER_MINUTE,
                 "windows": [{"label": "7d", "used_percent": 10.0}]},
                {"label": "elsewhere", "captured_at": 1_000_000,
                 "windows": [{"label": "7d", "used_percent": 1.0}]}
            ]
        });
        std::fs::write(&limits, doc.to_string()).unwrap();
        std::fs::write(
            temp.path().join("config.json"),
            r#"{"interval_minutes": 15}"#,
        )
        .unwrap();
        let accounts = accounts_with_failover();
        let now = 1000 + 60 * SECONDS_PER_MINUTE;
        let Pick { label, .. } = accounts
            .resolve_with_failover(Path::new("/somewhere"), temp.path(), now)
            .unwrap();
        assert_eq!(label, "mochi", "mochi's 10 min old reading keeps it fresh");
        // An account outside the chain does not make the chain's data fresh.
        let later = 1000 + 50 * SECONDS_PER_MINUTE + 31 * SECONDS_PER_MINUTE;
        let fresh_elsewhere_only = accounts
            .resolve_with_failover(Path::new("/somewhere"), temp.path(), later)
            .unwrap();
        assert_eq!(fresh_elsewhere_only.label, "simcity");
    }

    #[test]
    fn the_default_poll_interval_matches_ovm_limits() {
        let registry = include_str!("../../ovm-limits/src/registry.rs");
        let expected =
            format!("pub const DEFAULT_INTERVAL_MINUTES: u64 = {DEFAULT_POLL_INTERVAL_MINUTES};");
        assert!(
            registry.contains(&expected),
            "ovm-limits' DEFAULT_INTERVAL_MINUTES changed; update DEFAULT_POLL_INTERVAL_MINUTES"
        );
    }

    #[test]
    fn failover_config_round_trips_through_json() {
        let accounts = accounts_with_failover();
        let json = serde_json::to_string_pretty(&accounts).unwrap();
        let loaded: Accounts = serde_json::from_str(&json).unwrap();
        assert_eq!(loaded.failover.chain, accounts.failover.chain);
        assert_eq!(loaded.failover.threshold, accounts.failover.threshold);
    }

    #[test]
    fn empty_failover_is_omitted_from_json() {
        let accounts = Accounts::default();
        let json = serde_json::to_string(&accounts).unwrap();
        assert!(!json.contains("failover"), "{json}");
    }
}
