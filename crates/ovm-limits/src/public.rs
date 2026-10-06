//! The public feed: what a page on the open internet may say about these
//! accounts. An observed reset log — every early reset, reset-credit spend
//! and lifted limit the poller saw land, with the plan tier and the window's
//! usage before and after — and when the poller last ran and will run again.
//! Next to it, the resets the providers announced, from agentresets.com.
//!
//! Anonymised: an account appears only as a short key derived from a secret
//! salt, never by id, label, host or email. Written beside `limits.json` on
//! every merge so a hook can upload it as-is.

use crate::announcements::{Announcement, Announcements, Attribution};
use crate::events::{Event, Kind};
use crate::registry::Provider;
use crate::snapshot::Merged;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::collections::BTreeSet;

pub const SCHEMA: &str = "ovm-limits/resets-v2";
/// How many resets the feed carries: the newest, across kinds.
const RECENT_RESETS: usize = 50;
/// Resets on two accounts this close together landed together.
const SIMULTANEOUS_SECONDS: u64 = 90;
/// How far an announcement may be from a reset and still be its cause.
const ANNOUNCED_WITHIN_SECONDS: u64 = 36 * 3600;
/// How far back the feed lists announcements on their own.
const ANNOUNCEMENT_DAYS: u64 = 30;
const DAY_SECONDS: u64 = 86_400;
/// Hex characters of the account key.
const ACCOUNT_KEY_CHARS: usize = 8;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Feed {
    pub schema: String,
    pub generated_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_poll_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_poll_at: Option<u64>,
    pub poll_every_minutes: u64,
    /// Which products are being watched, so a page can say so.
    pub watching: Vec<Provider>,
    /// How many days the event log behind the feed goes back.
    pub history_days: u64,
    /// Observed resets, newest first.
    pub resets: Vec<PublicReset>,
    /// Announced resets of the last thirty days, and any older one an
    /// observed reset points at. Oldest first.
    #[serde(default)]
    pub announcements: Vec<Announcement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attribution: Option<Attribution>,
}

/// What kind of reset landed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetKind {
    /// Usage fell before the window was due: the provider reset it.
    EarlyReset,
    /// The account spent one of its own reset credits.
    CreditReset,
    /// A window that had reached 100% rolled over, so the account works again.
    LimitLifted,
}

impl ResetKind {
    fn of(kind: Kind) -> Option<Self> {
        match kind {
            Kind::SurpriseReset => Some(Self::EarlyReset),
            Kind::CreditReset => Some(Self::CreditReset),
            Kind::LimitLifted => Some(Self::LimitLifted),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicReset {
    /// When the poll that noticed it ran.
    pub at: u64,
    pub provider: Provider,
    pub kind: ResetKind,
    pub window_id: String,
    pub window_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
    /// The plan tier (`pro`, `max_20x`, `team_standard`…), when known.
    pub plan: Option<String>,
    /// The window's usage on the poll before, and on this one, in whole
    /// percent.
    pub before_percent: Option<i64>,
    pub after_percent: Option<i64>,
    /// A stable anonymous key for the account, so a reader can tell one
    /// account's resets from another's and nothing more.
    pub account_key: String,
    /// Accounts of this provider, this one included, with a reset within
    /// ±90 s: several is the provider, one is likely the account itself.
    pub simultaneous: usize,
    /// The id of the announcement this reset most likely answers to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub announced_ref: Option<String>,
}

/// Pure: the merged view, the whole event log (oldest first), the cached
/// announcements and the salt → the feed.
pub fn feed(
    merged: &Merged,
    events: &[Event],
    announcements: Option<&Announcements>,
    salt: &str,
) -> Feed {
    let mut watching: Vec<Provider> = merged.accounts.iter().map(|a| a.provider).collect();
    watching.sort_by_key(|p| p.to_string());
    watching.dedup();
    let landed: Vec<(&Event, ResetKind)> = events
        .iter()
        .filter_map(|e| ResetKind::of(e.kind).map(|kind| (e, kind)))
        .collect();
    let announced: &[Announcement] = announcements.map_or(&[], |a| a.items.as_slice());
    let mut resets: Vec<PublicReset> = landed
        .iter()
        .map(|&(e, kind)| {
            let account = merged.accounts.iter().find(|a| a.id == e.account_id);
            PublicReset {
                at: e.at,
                provider: e.provider,
                kind,
                window_id: e.window_id.clone().unwrap_or_default(),
                window_label: e.window_label.clone().unwrap_or_default(),
                window_minutes: account
                    .into_iter()
                    .flat_map(|a| a.windows.iter())
                    .find(|w| Some(&w.id) == e.window_id.as_ref())
                    .and_then(|w| w.window_minutes),
                plan: account
                    .and_then(|a| a.plan.clone().or_else(|| a.tier.clone()))
                    .map(|plan| public_plan(&plan)),
                before_percent: e.previous_used_percent.map(whole),
                after_percent: e.used_percent.map(whole),
                account_key: account_key(salt, &e.host, &e.account_id),
                simultaneous: simultaneous(e, &landed),
                announced_ref: if kind == ResetKind::EarlyReset {
                    nearest(e, announced).map(|a| a.id.clone())
                } else {
                    None
                },
            }
        })
        .collect();
    resets.sort_by_key(|r| std::cmp::Reverse(r.at));
    resets.truncate(RECENT_RESETS);
    let since = merged
        .generated_at
        .saturating_sub(ANNOUNCEMENT_DAYS * DAY_SECONDS);
    let referenced: BTreeSet<String> = resets
        .iter()
        .filter_map(|r| r.announced_ref.clone())
        .collect();
    let history_days = events.iter().map(|e| e.at).min().map_or(0, |oldest| {
        merged
            .generated_at
            .saturating_sub(oldest)
            .div_ceil(DAY_SECONDS)
    });
    Feed {
        schema: SCHEMA.into(),
        generated_at: merged.generated_at,
        last_poll_at: merged.last_poll_at,
        next_poll_at: merged.next_poll_at,
        poll_every_minutes: merged.interval_minutes,
        watching,
        history_days,
        resets,
        announcements: announced
            .iter()
            .filter(|a| a.ts >= since || referenced.contains(&a.id))
            .cloned()
            .collect(),
        attribution: announcements.map(|a| a.attribution.clone()),
    }
}

fn whole(percent: f64) -> i64 {
    percent.round() as i64
}

/// `default_claude_max_20x` → `max_20x`; Codex's `pro` stays `pro`.
fn public_plan(plan: &str) -> String {
    let plan = plan.trim().to_ascii_lowercase();
    let plan = plan.strip_prefix("default_").unwrap_or(&plan);
    plan.strip_prefix("claude_").unwrap_or(plan).to_string()
}

/// The first eight hex characters of sha256(salt, host, account id).
pub fn account_key(salt: &str, host: &str, account_id: &str) -> String {
    let digest = Sha256::new()
        .chain_update(salt.as_bytes())
        .chain_update([0])
        .chain_update(host.as_bytes())
        .chain_update([0])
        .chain_update(account_id.as_bytes())
        .finalize();
    let hex: String = digest.iter().map(|b| format!("{b:02x}")).collect();
    hex[..ACCOUNT_KEY_CHARS].to_string()
}

fn simultaneous(event: &Event, landed: &[(&Event, ResetKind)]) -> usize {
    landed
        .iter()
        .map(|(other, _)| other)
        .filter(|other| {
            other.provider == event.provider && other.at.abs_diff(event.at) <= SIMULTANEOUS_SECONDS
        })
        .map(|other| (other.host.as_str(), other.account_id.as_str()))
        .collect::<BTreeSet<_>>()
        .len()
}

/// The announcement for the same provider closest in time, within 36 hours.
fn nearest<'a>(event: &Event, announced: &'a [Announcement]) -> Option<&'a Announcement> {
    announced
        .iter()
        .filter(|a| a.provider == event.provider)
        .map(|a| (a.ts.abs_diff(event.at), a))
        .filter(|(distance, _)| *distance <= ANNOUNCED_WITHIN_SECONDS)
        .min_by_key(|(distance, _)| *distance)
        .map(|(_, a)| a)
}

/// The salt behind [`account_key`]: read from the limits dir, made once.
/// Random bytes from the system; should those be unreadable, the clock and
/// the process id, which still keep the key from being guessed off-machine.
pub fn salt(dirs: &crate::paths::LimitsDirs) -> String {
    let path = dirs.public_salt_file();
    if let Some(salt) = std::fs::read_to_string(&path)
        .ok()
        .map(|raw| raw.trim().to_string())
        .filter(|raw| !raw.is_empty())
    {
        return salt;
    }
    let mut bytes = [0u8; 16];
    let random = std::fs::File::open("/dev/urandom")
        .and_then(|mut file| std::io::Read::read_exact(&mut file, &mut bytes));
    if random.is_err() {
        let fallback = format!("{:?}-{}", std::time::SystemTime::now(), std::process::id());
        bytes.copy_from_slice(&Sha256::digest(fallback.as_bytes())[..16]);
    }
    let salt: String = bytes.iter().map(|b| format!("{b:02x}")).collect();
    let _ = crate::snapshot::write_atomic(&path, format!("{salt}\n").as_bytes());
    salt
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events;
    use crate::snapshot::{AccountSnapshot, Window};

    /// 2026-10-02T22:15:00Z.
    const LANDED: u64 = 1_790_979_300;
    const SALT: &str = "test-salt";

    fn account(provider: Provider, id: &str, label: &str, plan: Option<&str>) -> AccountSnapshot {
        let (window_id, window_label, minutes) = match provider {
            Provider::Codex => ("codex.primary", "weekly", 10_080),
            Provider::Claude => ("five_hour", "5h", 300),
        };
        AccountSnapshot {
            plan: plan.map(str::to_owned),
            windows: vec![Window {
                id: window_id.into(),
                label: window_label.into(),
                used_percent: 66.0,
                resets_at: Some(LANDED + 9_000),
                window_minutes: Some(minutes),
                last_reset_at: None,
            }],
            ..AccountSnapshot::empty(provider, id, Some(label.into()), "t")
        }
    }

    fn merged(accounts: Vec<AccountSnapshot>) -> Merged {
        Merged {
            schema: crate::snapshot::SCHEMA.into(),
            generated_at: LANDED + 600,
            host: "mini".into(),
            last_poll_at: Some(LANDED + 600),
            next_poll_at: Some(LANDED + 1_500),
            interval_minutes: 15,
            accounts,
            events: Vec::new(),
        }
    }

    fn landed(kind: Kind, provider: Provider, id: &str, at: u64, before: f64, after: f64) -> Event {
        let (window_id, window_label) = match provider {
            Provider::Codex => ("codex.primary", "weekly"),
            Provider::Claude => ("five_hour", "5h"),
        };
        Event {
            at,
            host: "mini".into(),
            kind,
            account_id: id.into(),
            label: Some(format!("{id}-label")),
            provider,
            window_id: Some(window_id.into()),
            window_label: Some(window_label.into()),
            used_percent: Some(after),
            previous_used_percent: Some(before),
            text: format!("{id} reset · person@example.com"),
            ..events::sample(at)
        }
    }

    fn fixture() -> (Merged, Vec<Event>) {
        let mut claude = account(Provider::Claude, "claude-1", "simcity", None);
        claude.tier = Some("default_claude_max_20x".into());
        let merged = merged(vec![
            claude,
            account(Provider::Codex, "codex-1", "work", Some("pro")),
            account(Provider::Codex, "codex-2", "home", Some("prolite")),
        ]);
        let mut threshold = landed(
            Kind::Threshold,
            Provider::Codex,
            "codex-1",
            LANDED - 30_000,
            60.0,
            71.0,
        );
        threshold.level = Some(70);
        let events = vec![
            threshold,
            landed(
                Kind::Reset,
                Provider::Claude,
                "claude-1",
                LANDED - 20_000,
                40.0,
                0.0,
            ),
            landed(
                Kind::LimitLifted,
                Provider::Claude,
                "claude-1",
                LANDED - 10_000,
                100.0,
                3.0,
            ),
            landed(
                Kind::CreditReset,
                Provider::Codex,
                "codex-2",
                LANDED - 5_000,
                87.0,
                1.0,
            ),
            landed(
                Kind::SurpriseReset,
                Provider::Codex,
                "codex-1",
                LANDED,
                100.0,
                0.0,
            ),
            landed(
                Kind::SurpriseReset,
                Provider::Codex,
                "codex-2",
                LANDED + 60,
                76.0,
                0.0,
            ),
        ];
        (merged, events)
    }

    #[test]
    fn the_feed_logs_every_reset_class_newest_first_with_plan_and_percentages() {
        let (merged, events) = fixture();
        let feed = feed(&merged, &events, None, SALT);
        assert_eq!(feed.schema, "ovm-limits/resets-v2");
        let kinds: Vec<ResetKind> = feed.resets.iter().map(|r| r.kind).collect();
        assert_eq!(
            kinds,
            [
                ResetKind::EarlyReset,
                ResetKind::EarlyReset,
                ResetKind::CreditReset,
                ResetKind::LimitLifted
            ],
            "scheduled rollovers and thresholds stay off"
        );
        let first = &feed.resets[1];
        assert_eq!(first.at, LANDED);
        assert_eq!(first.plan.as_deref(), Some("pro"));
        assert_eq!(
            (first.before_percent, first.after_percent),
            (Some(100), Some(0))
        );
        assert_eq!(first.window_label, "weekly");
        assert_eq!(first.window_minutes, Some(10_080));
        assert_eq!(
            feed.resets[3].plan.as_deref(),
            Some("max_20x"),
            "Claude's tier, shortened"
        );
        assert_eq!(feed.resets[2].plan.as_deref(), Some("prolite"));
        assert_eq!(feed.watching, [Provider::Claude, Provider::Codex]);
        assert_eq!(feed.history_days, 1);
        assert_eq!(feed.next_poll_at, Some(LANDED + 1_500));
        assert_eq!(feed.poll_every_minutes, 15);
        assert!(feed.announcements.is_empty() && feed.attribution.is_none());
    }

    #[test]
    fn the_feed_keeps_the_newest_fifty_across_kinds() {
        let (merged, _) = fixture();
        let events: Vec<Event> = (0..60)
            .map(|i| {
                let kind = if i % 2 == 0 {
                    Kind::SurpriseReset
                } else {
                    Kind::LimitLifted
                };
                landed(
                    kind,
                    Provider::Codex,
                    "codex-1",
                    LANDED + i * 1_000,
                    50.0,
                    0.0,
                )
            })
            .collect();
        let feed = feed(&merged, &events, None, SALT);
        assert_eq!(feed.resets.len(), RECENT_RESETS);
        assert_eq!(feed.resets[0].at, LANDED + 59_000);
        assert_eq!(feed.resets[49].at, LANDED + 10_000);
    }

    #[test]
    fn resets_within_ninety_seconds_on_one_provider_count_as_simultaneous() {
        let (merged, mut events) = fixture();
        // A second window on the same account in the same poll is still one
        // account; a Claude reset in the same minute is another provider.
        let mut five_hour = landed(
            Kind::SurpriseReset,
            Provider::Codex,
            "codex-1",
            LANDED,
            40.0,
            0.0,
        );
        five_hour.window_id = Some("codex.secondary".into());
        events.push(five_hour);
        events.push(landed(
            Kind::SurpriseReset,
            Provider::Claude,
            "claude-1",
            LANDED + 30,
            50.0,
            0.0,
        ));
        events.push(landed(
            Kind::SurpriseReset,
            Provider::Codex,
            "codex-3",
            LANDED + 91,
            50.0,
            0.0,
        ));
        let feed = feed(&merged, &events, None, SALT);
        let at = |at: u64, provider: Provider| {
            feed.resets
                .iter()
                .find(|r| r.at == at && r.provider == provider)
                .unwrap()
                .simultaneous
        };
        assert_eq!(
            at(LANDED, Provider::Codex),
            2,
            "codex-1 and codex-2, not codex-3"
        );
        assert_eq!(
            at(LANDED + 60, Provider::Codex),
            3,
            "codex-3 is 31 s after it"
        );
        assert_eq!(at(LANDED + 30, Provider::Claude), 1);
        assert_eq!(
            at(LANDED - 5_000, Provider::Codex),
            1,
            "a credit spent alone"
        );
    }

    #[test]
    fn no_label_host_id_or_email_reaches_the_feed() {
        let (merged, events) = fixture();
        let announcements = crate::announcements::parse(crate::announcements::FIXTURE).unwrap();
        let feed = feed(&merged, &events, Some(&announcements), SALT);
        let json = serde_json::to_string(&feed).unwrap();
        for private in [
            "simcity",
            "work",
            "home\"",
            "claude-1",
            "codex-1",
            "codex-2",
            "mini",
            "@example.com",
            "used_percent",
            "\"host\"",
            SALT,
        ] {
            assert!(!json.contains(private), "{private} leaked: {json}");
        }
        let keys: BTreeSet<&str> = feed.resets.iter().map(|r| r.account_key.as_str()).collect();
        assert_eq!(keys.len(), 3, "one key per account");
        assert!(keys.iter().all(|k| k.len() == ACCOUNT_KEY_CHARS));
        assert_eq!(
            feed.resets[1].account_key,
            account_key(SALT, "mini", "codex-1")
        );
        assert_ne!(
            account_key(SALT, "mini", "codex-1"),
            account_key("another-salt", "mini", "codex-1"),
            "the key depends on the secret"
        );
    }

    #[test]
    fn early_resets_point_at_the_nearest_announcement_within_36_hours() {
        let (merged, mut events) = fixture();
        // 2026-08-06, five days after an old banked announcement: too far.
        events.push(landed(
            Kind::SurpriseReset,
            Provider::Codex,
            "codex-1",
            1_786_010_400,
            30.0,
            0.0,
        ));
        let announcements = crate::announcements::parse(crate::announcements::FIXTURE).unwrap();
        let feed = feed(&merged, &events, Some(&announcements), SALT);
        let by_at = |at: u64| feed.resets.iter().find(|r| r.at == at).unwrap();
        assert_eq!(
            by_at(LANDED).announced_ref.as_deref(),
            Some("c-1002"),
            "5h 15m later"
        );
        assert_eq!(by_at(LANDED + 60).announced_ref.as_deref(), Some("c-1002"));
        assert_eq!(
            by_at(LANDED - 5_000).announced_ref,
            None,
            "a credit spend is the account's own"
        );
        assert_eq!(by_at(1_786_010_400).announced_ref, None);
        let ids: Vec<&str> = feed.announcements.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["a-0922", "c-1002"],
            "the last thirty days; August is out"
        );
        let attribution = feed.attribution.unwrap();
        assert_eq!(attribution.name, "agentresets.com");
        assert!(attribution.note.contains("attribution required"));
    }

    #[test]
    fn the_salt_is_made_once_and_kept() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = crate::paths::LimitsDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        let first = salt(&dirs);
        assert_eq!(first.len(), 32);
        assert_eq!(salt(&dirs), first);
    }
}
