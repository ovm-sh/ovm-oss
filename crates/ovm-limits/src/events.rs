//! What changed between two polls of the same account: a window rolling over
//! on schedule, a window reset out of schedule, a threshold crossed, a poll
//! that started failing or stopped. Events are the part of a poll worth
//! telling someone about, so they are kept in a log of their own and handed
//! to the hooks one at a time.
//!
//! The one that matters is the surprise reset. Inside a window usage only
//! climbs — every turn adds to it, including the poll's own — so usage that
//! FELL while the window's reset time was still ahead means the provider
//! reset it early, the way Anthropic does after an incident. That is news;
//! the scheduled rollover is the clock.

use crate::paths::LimitsDirs;
use crate::registry::Provider;
use crate::snapshot::{self, AccountSnapshot};
use crate::Result;
use serde::{Deserialize, Serialize};

/// Usage levels worth a word when crossed upward.
pub const THRESHOLDS: [u8; 3] = [70, 90, 100];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// A window rolled over on schedule: its reset time had passed, or moved
    /// forward past where it was.
    Reset,
    /// Usage fell while the window's reset time was still ahead: the
    /// provider reset it early.
    SurpriseReset,
    /// Usage fell early because the account spent one of its own reset
    /// credits. Same shape as a [`Kind::SurpriseReset`], but the cause is the
    /// person at the keyboard, so it is news to record and not news to alarm
    /// anyone with.
    CreditReset,
    /// A window that had reached 100% rolled over on schedule, so the account
    /// can work again. Any other rollover is only the clock; the end of an
    /// outage is news, and it is the half of "limits hit" that tells anyone
    /// waiting on the account that they can stop.
    LimitLifted,
    /// Usage crossed one of [`THRESHOLDS`] on the way up.
    Threshold,
    /// The poll of this account missed once; the next poll will say whether
    /// it was a flap or a fault.
    Retrying,
    /// The poll of this account is failing: twice in a row, or its home has
    /// no login in it.
    Failed,
    /// …and stopped failing.
    Recovered,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
    /// Unix epoch seconds when the poll that noticed this ran.
    pub at: u64,
    pub host: String,
    pub kind: Kind,
    pub account_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    pub provider: Provider,
    /// The window concerned, for resets and thresholds.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub window_label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub used_percent: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous_used_percent: Option<f64>,
    /// The threshold crossed, for `threshold` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub level: Option<u8>,
    /// When the window resets next, as of this poll.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resets_at: Option<u64>,
    /// The failure, for `failed` events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// The event as a person would read it in a notification.
    pub text: String,
}

impl Event {
    fn base(current: &AccountSnapshot, kind: Kind) -> Self {
        Self {
            at: current.captured_at,
            host: current.host.clone(),
            kind,
            account_id: current.id.clone(),
            label: current.label.clone(),
            provider: current.provider,
            window_id: None,
            window_label: None,
            used_percent: None,
            previous_used_percent: None,
            level: None,
            resets_at: None,
            error: None,
            text: String::new(),
        }
    }

    /// `claude-1 (simcity)`, the same shape everything else prints.
    fn who(&self) -> String {
        match &self.label {
            Some(label) => format!("{} ({label})", self.account_id),
            None => self.account_id.clone(),
        }
    }
}

/// Compare a fresh snapshot with the previous one for the same account.
/// Pure, and ordered: resets first, then thresholds, then the failure state.
///
/// `signed_in` is whether the account's home still holds a login. A single
/// failed poll is usually the machine being busy — a Codex app-server that
/// missed its window under load, a Claude session that was slow to start —
/// so the first miss is a `retrying`, the second in a row is a `failed`, and
/// a third says nothing more. A home with no login in it is `failed` at once,
/// and says what to do. The `failure_alerted` flag on the snapshot the caller
/// writes back is what makes the recovery an event too, and only then.
pub fn detect(
    previous: Option<&AccountSnapshot>,
    current: &mut AccountSnapshot,
    signed_in: bool,
) -> Vec<Event> {
    let mut events = Vec::new();
    let product = current.provider.display_name();
    current.consecutive_failures = match (&current.error, previous) {
        (None, _) => 0,
        (Some(_), Some(previous)) => previous.consecutive_failures + 1,
        (Some(_), None) => 1,
    };
    current.failure_alerted =
        current.error.is_some() && previous.is_some_and(|p| p.failure_alerted && p.error.is_some());
    let Some(previous) = previous else {
        return events;
    };
    // Spending a reset credit empties a window exactly the way an unannounced
    // provider reset does, so the drop alone cannot tell them apart. The
    // balance can: it only ever falls when the account spends one. Checked
    // once, because one credit clears every window at once.
    let credit_spent = match (
        previous.reset_credits_available,
        current.reset_credits_available,
    ) {
        (Some(before), Some(now)) => now < before,
        _ => false,
    };
    if current.error.is_none() {
        for window in &current.windows {
            let Some(before) = previous.windows.iter().find(|w| w.id == window.id) else {
                continue;
            };
            let was_due = before
                .resets_at
                .is_none_or(|then| then <= current.captured_at);
            let moved_forward = match (before.resets_at, window.resets_at) {
                (Some(then), Some(now)) => now > then,
                _ => false,
            };
            // A point of rounding is not a drop; Claude reports whole
            // percentages and the poll's own turn is worth about that.
            let fell = window.used_percent + 1.0 < before.used_percent;
            let kind = if fell && !was_due {
                Some(if credit_spent {
                    Kind::CreditReset
                } else {
                    Kind::SurpriseReset
                })
            } else if was_due && (fell || moved_forward) {
                // Out of a spent window and actually down again: the outage is
                // over. A window still reading 100 after rolling is not back.
                if before.used_percent >= 100.0 && window.used_percent < 100.0 {
                    Some(Kind::LimitLifted)
                } else {
                    Some(Kind::Reset)
                }
            } else {
                None
            };
            if let Some(kind) = kind {
                let mut event = Event::base(current, kind);
                event.window_id = Some(window.id.clone());
                event.window_label = Some(window.label.clone());
                event.used_percent = Some(window.used_percent);
                event.previous_used_percent = Some(before.used_percent);
                event.resets_at = window.resets_at;
                let what = match kind {
                    Kind::SurpriseReset => "window reset early",
                    Kind::CreditReset => "window cleared by a reset credit",
                    Kind::LimitLifted => "window is back",
                    _ => "window rolled over",
                };
                event.text = format!(
                    "{product} {} {what} · {} · was {}%, now {}%",
                    window.label,
                    event.who(),
                    before.used_percent.round() as i64,
                    window.used_percent.round() as i64
                );
                events.push(event);
            }
            for level in THRESHOLDS {
                let bar = f64::from(level);
                if window.used_percent >= bar && before.used_percent < bar {
                    let mut event = Event::base(current, Kind::Threshold);
                    event.window_id = Some(window.id.clone());
                    event.window_label = Some(window.label.clone());
                    event.used_percent = Some(window.used_percent);
                    event.previous_used_percent = Some(before.used_percent);
                    event.level = Some(level);
                    event.resets_at = window.resets_at;
                    event.text = format!(
                        "{product} {} window at {}% · {}{}",
                        window.label,
                        window.used_percent.round() as i64,
                        event.who(),
                        match window.resets_at {
                            Some(at) if at > current.captured_at => format!(
                                " · resets in {}",
                                crate::show::human(at - current.captured_at)
                            ),
                            _ => String::new(),
                        }
                    );
                    events.push(event);
                }
            }
        }
    }
    match &current.error {
        Some(error) if current.consecutive_failures <= 2 => {
            let first_miss = current.consecutive_failures == 1;
            let kind = if signed_in && first_miss {
                Kind::Retrying
            } else {
                Kind::Failed
            };
            let mut event = Event::base(current, kind);
            event.error = Some(error.clone());
            event.text = if !signed_in {
                format!(
                    "{product} signed out · {} · run: ovm limits login {}",
                    event.who(),
                    current.id
                )
            } else if first_miss {
                format!(
                    "{product} poll missed · {} · {error} · retrying",
                    event.who()
                )
            } else {
                format!(
                    "{product} poll still failing · {} · {error} (2 polls in a row)",
                    event.who()
                )
            };
            events.push(event);
            current.failure_alerted = true;
        }
        Some(_) => {}
        None if previous.failure_alerted => {
            let mut event = Event::base(current, Kind::Recovered);
            event.text = format!("{product} poll recovered · {}", event.who());
            events.push(event);
        }
        None => {}
    }
    events
}

/// Append to the log. One JSON object per line, oldest first.
pub fn append(dirs: &LimitsDirs, events: &[Event]) -> Result<()> {
    if events.is_empty() {
        return Ok(());
    }
    use std::io::Write;
    let path = dirs.events_file();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    for event in events {
        serde_json::to_writer(&mut file, event)?;
        file.write_all(b"\n")?;
    }
    Ok(())
}

/// Every event since `since` (epoch seconds), oldest first. Unreadable lines
/// are skipped: a log that was being appended when the machine went down
/// should not take the rest of the history with it.
pub fn load(dirs: &LimitsDirs, since: u64) -> Result<Vec<Event>> {
    let raw = match std::fs::read_to_string(dirs.events_file()) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(error.into()),
    };
    Ok(raw
        .lines()
        .filter_map(|line| serde_json::from_str::<Event>(line).ok())
        .filter(|event| event.at >= since)
        .collect())
}

/// The most recent `count` events, newest last.
pub fn recent(dirs: &LimitsDirs, count: usize) -> Result<Vec<Event>> {
    let mut all = load(dirs, 0)?;
    let keep = all.len().saturating_sub(count);
    Ok(all.split_off(keep))
}

/// Surprise resets on their own, for the public feed and the notifier.
pub fn surprises(events: &[Event]) -> Vec<&Event> {
    events
        .iter()
        .filter(|e| e.kind == Kind::SurpriseReset)
        .collect()
}

/// `2h 13m ago · Claude 5h window reset · claude-1 (simcity)`
pub fn line(event: &Event, now: u64) -> String {
    format!(
        "{:>9} · {}",
        format!("{} ago", crate::show::human(now.saturating_sub(event.at))),
        event.text
    )
}

/// Seed the list of events with a synthetic reset, for `hook test`.
pub fn sample(now: u64) -> Event {
    Event {
        at: now,
        host: snapshot::hostname(),
        kind: Kind::SurpriseReset,
        account_id: "claude-1".into(),
        label: Some("test".into()),
        provider: Provider::Claude,
        window_id: Some("five_hour".into()),
        window_label: Some("5h".into()),
        used_percent: Some(0.0),
        previous_used_percent: Some(64.0),
        level: None,
        resets_at: Some(now + 5 * 3600),
        error: None,
        text: "Claude Code 5h window reset early · claude-1 (test) · was 64%, now 0% (hook test)"
            .into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Window;

    fn window(id: &str, used: f64, resets_at: u64) -> Window {
        Window {
            id: id.into(),
            label: match id {
                "five_hour" => "5h".into(),
                _ => "7d".into(),
            },
            used_percent: used,
            resets_at: Some(resets_at),
            window_minutes: None,
            last_reset_at: None,
        }
    }

    fn snapshot(at: u64, windows: Vec<Window>, error: Option<&str>) -> AccountSnapshot {
        AccountSnapshot {
            captured_at: at,
            host: "m5".into(),
            windows,
            error: error.map(String::from),
            ..AccountSnapshot::empty(Provider::Claude, "claude-1", Some("simcity".into()), "test")
        }
    }

    fn diff(previous: Option<&AccountSnapshot>, current: &mut AccountSnapshot) -> Vec<Event> {
        detect(previous, current, true)
    }

    #[test]
    fn the_first_poll_has_nothing_to_compare_against() {
        let mut current = snapshot(1_000, vec![window("five_hour", 5.0, 2_000)], None);
        assert!(diff(None, &mut current).is_empty());
        let mut broken = snapshot(1_000, vec![], Some("boom"));
        assert!(diff(None, &mut broken).is_empty());
        assert_eq!(broken.consecutive_failures, 1);
    }

    #[test]
    fn a_window_that_was_due_and_rolled_over_is_the_clock() {
        let before = snapshot(1_000, vec![window("five_hour", 64.0, 1_500)], None);
        let mut after = snapshot(1_600, vec![window("five_hour", 3.0, 19_600)], None);
        let events = diff(Some(&before), &mut after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Reset);
        assert_eq!(events[0].window_label.as_deref(), Some("5h"));
        assert_eq!(
            events[0].text,
            "Claude Code 5h window rolled over · claude-1 (simcity) · was 64%, now 3%"
        );
        // Due and fallen, even with the reset time not yet moved, is the same.
        let mut stale = snapshot(1_600, vec![window("five_hour", 10.0, 1_500)], None);
        assert_eq!(diff(Some(&before), &mut stale)[0].kind, Kind::Reset);
    }

    #[test]
    fn usage_that_fell_before_the_window_was_due_is_a_surprise_reset() {
        let before = snapshot(1_000, vec![window("five_hour", 64.0, 9_000)], None);
        let mut after = snapshot(1_600, vec![window("five_hour", 2.0, 9_000)], None);
        let events = diff(Some(&before), &mut after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::SurpriseReset);
        assert_eq!(
            events[0].text,
            "Claude Code 5h window reset early · claude-1 (simcity) · was 64%, now 2%"
        );
        // The provider may hand out a fresh reset time with it; still early.
        let mut renewed = snapshot(1_600, vec![window("five_hour", 0.0, 19_600)], None);
        assert_eq!(
            diff(Some(&before), &mut renewed)[0].kind,
            Kind::SurpriseReset
        );
        // Usage rising inside the window is nothing, and a point of rounding
        // down is not a reset.
        let mut busier = snapshot(1_200, vec![window("five_hour", 66.0, 9_000)], None);
        assert!(diff(Some(&before), &mut busier).is_empty());
        let mut rounding = snapshot(1_200, vec![window("five_hour", 63.5, 9_000)], None);
        assert!(diff(Some(&before), &mut rounding).is_empty());
    }

    #[test]
    fn a_window_emptied_by_a_spent_reset_credit_is_not_a_surprise() {
        let with_credits = |at: u64, used: f64, credits: u64| AccountSnapshot {
            reset_credits_available: Some(credits),
            ..snapshot(at, vec![window("five_hour", used, 9_000)], None)
        };
        // One credit spent between the two polls: the drop is the operator's
        // own doing, so it is recorded and never alarmed about.
        let before = with_credits(1_000, 100.0, 1);
        let mut after = with_credits(1_600, 0.0, 0);
        let events = diff(Some(&before), &mut after);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::CreditReset);
        assert_eq!(
            events[0].text,
            "Claude Code 5h window cleared by a reset credit · claude-1 (simcity) \
             · was 100%, now 0%"
        );
        // A credit reset is the operator's, so it never reaches the public
        // feed of provider resets.
        assert!(surprises(&events).is_empty());

        // The same drop with the balance untouched is still the provider's.
        let mut untouched = with_credits(1_600, 0.0, 1);
        assert_eq!(
            diff(Some(&before), &mut untouched)[0].kind,
            Kind::SurpriseReset
        );
        // A balance that grew (a credit was granted) is not a spend either.
        let mut granted = with_credits(1_600, 0.0, 2);
        assert_eq!(
            diff(Some(&before), &mut granted)[0].kind,
            Kind::SurpriseReset
        );
        // A provider that reports no balance at all cannot excuse the drop.
        let blind_before = snapshot(1_000, vec![window("five_hour", 100.0, 9_000)], None);
        let mut blind_after = snapshot(1_600, vec![window("five_hour", 0.0, 9_000)], None);
        assert_eq!(
            diff(Some(&blind_before), &mut blind_after)[0].kind,
            Kind::SurpriseReset
        );
        // Spending a credit does not invent a reset where usage held steady.
        let mut steady = with_credits(1_600, 100.0, 0);
        assert!(diff(Some(&before), &mut steady).is_empty());
    }

    #[test]
    fn a_rollover_out_of_a_spent_window_is_the_limit_lifting() {
        let spent = snapshot(1_000, vec![window("seven_day", 100.0, 1_500)], None);
        let mut back = snapshot(1_600, vec![window("seven_day", 0.0, 9_000)], None);
        let events = diff(Some(&spent), &mut back);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::LimitLifted);
        assert_eq!(
            events[0].text,
            "Claude Code 7d window is back · claude-1 (simcity) · was 100%, now 0%"
        );
        // Rolled but still reading 100: not back, just the clock.
        let mut still = snapshot(1_600, vec![window("seven_day", 100.0, 9_000)], None);
        assert_eq!(diff(Some(&spent), &mut still)[0].kind, Kind::Reset);
        // Cleared early by the provider is still a surprise, not a lifting.
        let early = snapshot(1_000, vec![window("seven_day", 100.0, 9_000)], None);
        let mut cleared = snapshot(1_600, vec![window("seven_day", 0.0, 9_000)], None);
        assert_eq!(
            diff(Some(&early), &mut cleared)[0].kind,
            Kind::SurpriseReset
        );
        // And a lifting is the operator's news, never the public feed's.
        assert!(surprises(&events).is_empty());
    }

    #[test]
    fn thresholds_fire_once_each_on_the_way_up() {
        let before = snapshot(1_000, vec![window("seven_day", 65.0, 9_000)], None);
        let mut after = snapshot(1_100, vec![window("seven_day", 92.0, 9_000)], None);
        let events = diff(Some(&before), &mut after);
        let levels: Vec<u8> = events.iter().filter_map(|e| e.level).collect();
        assert_eq!(levels, [70, 90], "both bars were crossed in one step");
        assert!(
            events[0].text.contains("7d window at 92%"),
            "{}",
            events[0].text
        );
        assert!(
            events[0].text.contains("resets in 2h 11m"),
            "{}",
            events[0].text
        );
        let mut same = snapshot(1_200, vec![window("seven_day", 93.0, 9_000)], None);
        assert!(
            diff(Some(&after), &mut same).is_empty(),
            "no re-firing above the bar"
        );
    }

    /// The first miss says "retrying", the second says "still failing", a
    /// third says nothing more, and the recovery closes the thread.
    #[test]
    fn a_miss_is_a_retry_then_a_failure_then_silence_then_a_recovery() {
        let ok = snapshot(1_000, vec![window("five_hour", 5.0, 2_000)], None);
        let mut first = snapshot(1_100, vec![], Some("did not answer within 30s"));
        let events = diff(Some(&ok), &mut first);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Retrying);
        assert_eq!(
            events[0].text,
            "Claude Code poll missed · claude-1 (simcity) · did not answer within 30s · retrying"
        );
        assert_eq!(first.consecutive_failures, 1);
        assert!(first.failure_alerted);
        // A flap that recovers on the next poll closes with a recovery.
        let mut quick = snapshot(1_200, vec![window("five_hour", 5.0, 2_000)], None);
        assert_eq!(diff(Some(&first), &mut quick)[0].kind, Kind::Recovered);

        let mut second = snapshot(1_200, vec![], Some("did not answer within 30s"));
        let events = diff(Some(&first), &mut second);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Failed);
        assert!(
            events[0].text.contains("still failing"),
            "{}",
            events[0].text
        );
        assert!(second.failure_alerted);
        // A third miss says nothing more.
        let mut third = snapshot(1_300, vec![], Some("still nothing"));
        assert!(diff(Some(&second), &mut third).is_empty());
        assert_eq!(third.consecutive_failures, 3);
        assert!(
            third.failure_alerted,
            "the alert carries through the streak"
        );
        let mut back = snapshot(1_400, vec![window("five_hour", 5.0, 2_000)], None);
        let events = diff(Some(&third), &mut back);
        assert_eq!(events[0].kind, Kind::Recovered);
        assert_eq!(back.consecutive_failures, 0);
        assert!(!back.failure_alerted);
    }

    /// A home with no login in it is not a hiccup: say so at once, and say
    /// what to do.
    #[test]
    fn a_signed_out_home_is_news_on_the_first_miss() {
        let ok = snapshot(1_000, vec![window("five_hour", 5.0, 2_000)], None);
        let mut broken = snapshot(1_100, vec![], Some("not signed in"));
        let events = detect(Some(&ok), &mut broken, false);
        assert_eq!(events.len(), 1);
        assert_eq!(events[0].kind, Kind::Failed);
        assert_eq!(
            events[0].text,
            "Claude Code signed out · claude-1 (simcity) · run: ovm limits login claude-1"
        );
        assert!(broken.failure_alerted);
    }

    #[test]
    fn the_log_appends_and_reads_back_since_a_moment() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        let older = sample(1_000);
        let newer = sample(2_000);
        assert_eq!(surprises(std::slice::from_ref(&older)).len(), 1);
        append(&dirs, std::slice::from_ref(&older)).unwrap();
        append(&dirs, std::slice::from_ref(&newer)).unwrap();
        assert_eq!(load(&dirs, 0).unwrap(), vec![older, newer.clone()]);
        assert_eq!(load(&dirs, 1_500).unwrap(), vec![newer.clone()]);
        assert_eq!(recent(&dirs, 1).unwrap(), vec![newer]);
        assert_eq!(recent(&dirs, 10).unwrap().len(), 2);
    }
}
