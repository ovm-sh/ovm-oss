//! The public feed: what a page on the open internet may say about these
//! accounts. Surprise resets — the provider clearing a window early — and
//! when the poller last ran and will run again. Never a usage percentage,
//! never an account name. Written beside `limits.json` on every merge so a
//! hook can upload it as-is.

use crate::events::{self, Event};
use crate::registry::Provider;
use crate::snapshot::Merged;
use serde::{Deserialize, Serialize};

pub const SCHEMA: &str = "ovm-limits/resets-v1";
/// How many resets the feed carries.
const RECENT_RESETS: usize = 50;

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
    /// Surprise resets, oldest first.
    pub resets: Vec<PublicReset>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PublicReset {
    /// When the poll that noticed it ran.
    pub at: u64,
    pub provider: Provider,
    pub window_id: String,
    pub window_label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_minutes: Option<u64>,
}

/// Pure: the merged view and the event log → the feed.
pub fn feed(merged: &Merged, events: &[Event]) -> Feed {
    let mut watching: Vec<Provider> = merged.accounts.iter().map(|a| a.provider).collect();
    watching.sort_by_key(|p| p.to_string());
    watching.dedup();
    let mut resets: Vec<PublicReset> = events::surprises(events)
        .into_iter()
        .map(|e| PublicReset {
            at: e.at,
            provider: e.provider,
            window_id: e.window_id.clone().unwrap_or_default(),
            window_label: e.window_label.clone().unwrap_or_default(),
            window_minutes: merged
                .accounts
                .iter()
                .filter(|a| a.id == e.account_id)
                .flat_map(|a| a.windows.iter())
                .find(|w| Some(&w.id) == e.window_id.as_ref())
                .and_then(|w| w.window_minutes),
        })
        .collect();
    let keep = resets.len().saturating_sub(RECENT_RESETS);
    resets.drain(..keep);
    Feed {
        schema: SCHEMA.into(),
        generated_at: merged.generated_at,
        last_poll_at: merged.last_poll_at,
        next_poll_at: merged.next_poll_at,
        poll_every_minutes: merged.interval_minutes,
        watching,
        resets,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::Kind;
    use crate::snapshot::{AccountSnapshot, Window};

    #[test]
    fn the_feed_carries_surprise_resets_and_timing_and_nothing_private() {
        let merged = Merged {
            schema: crate::snapshot::SCHEMA.into(),
            generated_at: 5_000,
            host: "mini".into(),
            last_poll_at: Some(5_000),
            next_poll_at: Some(5_900),
            interval_minutes: 15,
            accounts: vec![AccountSnapshot {
                windows: vec![Window {
                    id: "five_hour".into(),
                    label: "5h".into(),
                    used_percent: 66.0,
                    resets_at: Some(9_000),
                    window_minutes: Some(300),
                    last_reset_at: Some(4_000),
                }],
                ..AccountSnapshot::empty(Provider::Claude, "claude-1", Some("simcity".into()), "t")
            }],
            events: Vec::new(),
        };
        let mut scheduled = events::sample(4_500);
        scheduled.kind = Kind::Reset;
        let feed = feed(&merged, &[events::sample(4_000), scheduled]);
        let json = serde_json::to_string(&feed).unwrap();
        for private in ["used_percent", "66", "simcity", "claude-1", "mini"] {
            assert!(!json.contains(private), "{private} leaked: {json}");
        }
        assert_eq!(
            feed.resets.len(),
            1,
            "scheduled rollovers stay off the feed"
        );
        assert_eq!(feed.resets[0].window_label, "5h");
        assert_eq!(feed.resets[0].window_minutes, Some(300));
        assert_eq!(feed.watching, [Provider::Claude]);
        assert_eq!(feed.next_poll_at, Some(5_900));
        assert_eq!(feed.poll_every_minutes, 15);
    }
}
