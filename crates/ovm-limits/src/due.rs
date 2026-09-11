//! When is an account worth a poll?
//!
//! The background agent ticks every five minutes so that resets are noticed
//! promptly, but a Claude poll is a real turn. So: Codex is always due (the
//! RPC is free), and a Claude account is due when it has never been polled,
//! when the configured interval has passed, or when one of its windows has
//! reset since the last snapshot — that last poll is what turns "the 5h window
//! should have reset by now" into "it did".

use crate::registry::{Account, Provider, Registry};
use crate::snapshot::AccountSnapshot;

pub fn is_due(
    account: &Account,
    last: Option<&AccountSnapshot>,
    interval_minutes: u64,
    now: u64,
) -> bool {
    if account.provider == Provider::Codex {
        return true;
    }
    let Some(last) = last else {
        return true;
    };
    if now.saturating_sub(last.captured_at) >= interval_minutes.saturating_mul(60) {
        return true;
    }
    last.windows.iter().any(|window| {
        window
            .resets_at
            .is_some_and(|reset| reset > last.captured_at && reset <= now)
    })
}

/// When one account next deserves a poll: Codex at the next tick, Claude when
/// its interval is up or a window resets, whichever comes first.
pub fn due_at(account: &Account, last: Option<&AccountSnapshot>, interval_minutes: u64) -> u64 {
    let Some(last) = last else {
        return 0;
    };
    if account.provider == Provider::Codex {
        return last.captured_at;
    }
    let by_interval = last.captured_at + interval_minutes * 60;
    last.windows
        .iter()
        .filter_map(|w| w.resets_at)
        .filter(|reset| *reset > last.captured_at)
        .chain(std::iter::once(by_interval))
        .min()
        .unwrap_or(by_interval)
}

/// When the background poller will next do anything: the earliest due time
/// across accounts, rounded up to the tick after it. `None` with nothing to
/// poll.
pub fn next_poll_at(
    registry: &Registry,
    snapshots: &[AccountSnapshot],
    tick_seconds: u64,
) -> Option<u64> {
    let last_poll = snapshots.iter().map(|s| s.captured_at).max()?;
    let earliest = registry
        .accounts
        .iter()
        .map(|account| {
            let last = snapshots
                .iter()
                .find(|s| s.provider == account.provider && s.id == account.id);
            due_at(account, last, registry.interval_minutes)
        })
        .min()?;
    let wait = earliest.saturating_sub(last_poll);
    let ticks = wait.div_ceil(tick_seconds).max(1);
    Some(last_poll + ticks * tick_seconds)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Window;

    fn claude() -> Account {
        Account {
            id: "claude-1".into(),
            provider: Provider::Claude,
            label: None,
            home: None,
            model: None,
        }
    }

    fn snapshot(captured_at: u64, resets_at: u64) -> AccountSnapshot {
        AccountSnapshot {
            captured_at,
            host: "test".into(),
            windows: vec![Window {
                id: "five_hour".into(),
                label: "5h".into(),
                used_percent: 40.0,
                resets_at: Some(resets_at),
                window_minutes: Some(300),
                last_reset_at: None,
            }],
            ..AccountSnapshot::empty(Provider::Claude, "claude-1", None, "test")
        }
    }

    #[test]
    fn never_polled_is_due() {
        assert!(is_due(&claude(), None, 60, 1_000));
    }

    #[test]
    fn inside_the_interval_with_no_reset_is_not_due() {
        let last = snapshot(1_000, 10_000);
        assert!(!is_due(&claude(), Some(&last), 60, 1_000 + 30 * 60));
    }

    #[test]
    fn the_interval_passing_makes_it_due() {
        let last = snapshot(1_000, 10_000);
        assert!(is_due(&claude(), Some(&last), 60, 1_000 + 60 * 60));
    }

    #[test]
    fn a_window_resetting_since_the_snapshot_makes_it_due_early() {
        let last = snapshot(1_000, 1_500);
        assert!(is_due(&claude(), Some(&last), 60, 1_501));
        // …but a reset that already lay in the past at capture time does not.
        let stale = snapshot(2_000, 1_500);
        assert!(!is_due(&claude(), Some(&stale), 60, 2_100));
    }

    #[test]
    fn a_failed_poll_retries_on_the_interval_not_every_tick() {
        let mut last = snapshot(1_000, 10_000);
        last.windows.clear();
        last.error = Some("boom".into());
        assert!(!is_due(&claude(), Some(&last), 60, 1_000 + 5 * 60));
        assert!(is_due(&claude(), Some(&last), 60, 1_000 + 60 * 60));
    }

    #[test]
    fn the_next_poll_is_the_earliest_due_time_rounded_up_to_a_tick() {
        let mut registry = Registry {
            interval_minutes: 60,
            ..Registry::default()
        };
        registry.accounts.push(claude());
        // Polled at t=1000, interval an hour, window resets at t=2500: the
        // reset comes first, and the tick after it is t=2500 → 2500 is on a
        // tick boundary from 1000 with 300s ticks.
        let snapshots = vec![snapshot(1_000, 2_500)];
        assert_eq!(next_poll_at(&registry, &snapshots, 300), Some(2_500));
        // A reset at t=2450 rounds up to the next tick, t=2500.
        assert_eq!(
            next_poll_at(&registry, &[snapshot(1_000, 2_450)], 300),
            Some(2_500)
        );
        // No reset ahead: the interval decides.
        assert_eq!(
            next_poll_at(&registry, &[snapshot(1_000, 500)], 300),
            Some(1_000 + 3_600)
        );
        // Codex is due every tick.
        let mut codex = claude();
        codex.provider = Provider::Codex;
        registry.accounts.push(codex);
        let mut codex_snapshot = snapshot(1_000, 9_000);
        codex_snapshot.provider = Provider::Codex;
        let both = vec![snapshot(1_000, 9_000), codex_snapshot];
        assert_eq!(next_poll_at(&registry, &both, 300), Some(1_300));
        assert!(next_poll_at(&registry, &[], 300).is_none());
    }

    #[test]
    fn codex_is_always_due_because_it_is_free() {
        let mut codex = claude();
        codex.provider = Provider::Codex;
        let last = snapshot(1_000, 10_000);
        assert!(is_due(&codex, Some(&last), 60, 1_001));
    }
}
