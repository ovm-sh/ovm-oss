//! Limit readings that natural sessions already produce, so a poll does not
//! have to spend a turn to learn them.
//!
//! - Claude: Echo, the statusline, writes each turn's `rate_limits` to
//!   `live/claude-<accountUuid>.json` (see `echo-statusline.py`).
//! - Codex: every session rollout under `$CODEX_HOME/sessions/Y/M/D/*.jsonl`
//!   logs a `rate_limits` event per turn, and — since 0.158 — names the login in
//!   its first line as `creator_account_id`. Older rollouts carry no account
//!   and are skipped rather than guessed at: the only other source is
//!   `auth.json`, which holds tokens, and this tool does not read those.
//!
//! A reading is keyed by the LOGIN it came from, not by a folder: it is tied to
//! a registered account through the `account_id` that account's own polls
//! recorded (see `merge`).

use crate::snapshot::Window;
use serde_json::Value;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

/// Rollouts reach hundreds of megabytes (222 MB seen on 2026-09-27); only the
/// tail is read for the latest reading, and only the head for the login.
const TAIL_BYTES: u64 = 1024 * 1024;
const HEAD_BYTES: usize = 256 * 1024;
/// How many of the newest rollouts per home are looked at.
const ROLLOUTS_PER_HOME: usize = 8;

#[derive(Debug, Clone, PartialEq)]
pub struct LiveReading {
    pub provider: crate::registry::Provider,
    /// The login: Claude `oauthAccount.accountUuid`, Codex `creator_account_id`.
    pub account_id: String,
    /// When the reading was taken, epoch seconds.
    pub at: u64,
    pub windows: Vec<Window>,
    /// Where it was read, for whoever debugs a merge.
    pub origin: PathBuf,
}

/// A reading whose reset time for a still-running window is further than this
/// from the account's own is taken to be another login's, not merged: the id a
/// natural session reports is fixed when the session starts, so a `/login` or
/// `codex login` mid-session can put one account's numbers under another's id.
const CROSS_CHECK_TOLERANCE_SECS: u64 = 3_600;

/// Inside one window usage only climbs; a reset is what brings it down, and a
/// reset brings it to about nothing. A live reading that shows a window lower
/// than known, but not near empty, before that window's reset is a session's
/// stale numbers (its transcript was touched before its next answer landed),
/// so that window keeps what it had.
const RESET_FLOOR_PERCENT: f64 = 5.0;

/// Window-id prefix of Codex's main meter (`codex.primary`, `codex.secondary`),
/// the one the app-server poll reads.
pub const CODEX_MAIN_LIMIT: &str = "codex.";

/// The source name a live reading gives the windows it refreshes.
pub const SOURCE: &str = "live";

/// The newest reading for `login` on `provider` taken after `after`.
pub fn newest_for<'a>(
    readings: &'a [LiveReading],
    provider: crate::registry::Provider,
    login: &str,
    after: u64,
) -> Option<&'a LiveReading> {
    readings
        .iter()
        .filter(|r| r.provider == provider && r.account_id == login)
        .filter(|r| r.at > after)
        .max_by_key(|r| r.at)
}

/// `last` with the windows `reading` names refreshed, or why it was refused.
pub fn apply(
    last: &crate::snapshot::AccountSnapshot,
    reading: &LiveReading,
) -> Result<crate::snapshot::AccountSnapshot, String> {
    use crate::snapshot::WindowSource;
    for fresh in &reading.windows {
        let Some(known) = last.windows.iter().find(|w| w.id == fresh.id) else {
            continue;
        };
        let (Some(known_reset), Some(fresh_reset)) = (known.resets_at, fresh.resets_at) else {
            continue;
        };
        // A window whose reset has already passed legitimately moved on.
        if known_reset <= reading.at {
            continue;
        }
        if known_reset.abs_diff(fresh_reset) > CROSS_CHECK_TOLERANCE_SECS {
            return Err(format!(
                "{} {}: the reading resets at {fresh_reset}, the account at {known_reset} — \
                 probably another login's numbers (a switch mid-session); not merged",
                last.id, fresh.id
            ));
        }
    }
    let mut next = last.clone();
    for window in &last.windows {
        next.window_sources
            .entry(window.id.clone())
            .or_insert_with(|| WindowSource {
                source: last.source.clone(),
                at: last.captured_at,
            });
    }
    for fresh in &reading.windows {
        let stale = last.windows.iter().any(|known| {
            known.id == fresh.id
                && known.resets_at.is_some_and(|reset| reset > reading.at)
                && fresh.used_percent + 1.0 < known.used_percent
                && fresh.used_percent > RESET_FLOOR_PERCENT
        });
        if stale {
            continue;
        }
        let id = fresh.id.clone();
        let mut fresh = fresh.clone();
        match next.windows.iter_mut().find(|w| w.id == id) {
            Some(slot) => {
                fresh.last_reset_at = slot.last_reset_at;
                *slot = fresh;
            }
            None => next.windows.push(fresh),
        }
        next.window_sources.insert(
            id,
            WindowSource {
                source: SOURCE.into(),
                at: reading.at,
            },
        );
    }
    next.captured_at = reading.at;
    next.source = SOURCE.into();
    next.error = None;
    Ok(next)
}

/// Every Claude reading Echo left in `live_dir`.
pub fn claude_readings(live_dir: &Path) -> Vec<LiveReading> {
    let Ok(entries) = std::fs::read_dir(live_dir) else {
        return Vec::new();
    };
    let mut readings = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let is_claude = path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("claude-") && n.ends_with(".json"));
        if !is_claude {
            continue;
        }
        let Some(doc) = std::fs::read_to_string(&path)
            .ok()
            .and_then(|raw| serde_json::from_str::<Value>(&raw).ok())
        else {
            continue;
        };
        let (Some(account_id), Some(at)) = (
            doc.get("account_id").and_then(Value::as_str),
            doc.get("at").and_then(Value::as_u64),
        ) else {
            continue;
        };
        let payload = serde_json::json!({ "rate_limits": doc.get("windows") });
        let windows = crate::claude::windows_from_statusline(&payload);
        if windows.is_empty() {
            continue;
        }
        readings.push(LiveReading {
            provider: crate::registry::Provider::Claude,
            account_id: account_id.to_string(),
            at,
            windows,
            origin: path,
        });
    }
    readings
}

/// The newest reading per login across the most recent rollouts in each home.
/// Rollouts last written at or before `after` are not opened: they cannot hold
/// anything newer than what is already merged, so a quiet tick reads nothing.
pub fn codex_readings(homes: &[PathBuf], after: u64) -> Vec<LiveReading> {
    // `after` is u64::MAX when there is no Codex account to read for; a
    // SystemTime that far out does not exist, and nothing is newer anyway.
    let Some(after) = std::time::UNIX_EPOCH.checked_add(std::time::Duration::from_secs(after))
    else {
        return Vec::new();
    };
    let mut readings: Vec<LiveReading> = Vec::new();
    for home in homes {
        for rollout in newest_rollouts(&home.join("sessions"), ROLLOUTS_PER_HOME, after) {
            let Some(reading) = read_rollout(&rollout) else {
                continue;
            };
            match readings
                .iter_mut()
                .find(|r| r.account_id == reading.account_id)
            {
                Some(existing) if existing.at >= reading.at => {}
                Some(existing) => *existing = reading,
                None => readings.push(reading),
            }
        }
    }
    readings
}

/// The homes whose sessions are worth reading: the default one, and
/// `$CODEX_HOME` when it points somewhere else.
pub fn default_codex_homes() -> Vec<PathBuf> {
    let mut homes = Vec::new();
    if let Some(home) = dirs::home_dir() {
        homes.push(home.join(".codex"));
    }
    if let Some(custom) = std::env::var_os("CODEX_HOME").filter(|v| !v.is_empty()) {
        let custom = PathBuf::from(custom);
        if !homes.contains(&custom) {
            homes.push(custom);
        }
    }
    homes
}

fn newest_rollouts(sessions: &Path, limit: usize, after: std::time::SystemTime) -> Vec<PathBuf> {
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = Vec::new();
    // Y/M/D/*.jsonl — walk only as deep as the layout goes.
    for year in sorted_dirs(sessions).into_iter().rev().take(2) {
        for month in sorted_dirs(&year).into_iter().rev().take(2) {
            for day in sorted_dirs(&month).into_iter().rev().take(3) {
                let Ok(entries) = std::fs::read_dir(&day) else {
                    continue;
                };
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.extension().is_some_and(|e| e == "jsonl") {
                        if let Ok(modified) = entry.metadata().and_then(|m| m.modified()) {
                            if modified <= after {
                                continue;
                            }
                            files.push((modified, path));
                        }
                    }
                }
            }
        }
    }
    files.sort_by_key(|(modified, _)| std::cmp::Reverse(*modified));
    files.into_iter().take(limit).map(|(_, p)| p).collect()
}

fn sorted_dirs(parent: &Path) -> Vec<PathBuf> {
    let Ok(entries) = std::fs::read_dir(parent) else {
        return Vec::new();
    };
    let mut dirs: Vec<PathBuf> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .collect();
    dirs.sort();
    dirs
}

/// The login from the rollout's first line and its latest `rate_limits` event.
pub fn read_rollout(path: &Path) -> Option<LiveReading> {
    let account_id = creator_account_id(path)?;
    let (at, rate_limits) = last_rate_limits(path)?;
    let windows = codex_windows(&rate_limits);
    if windows.is_empty() {
        return None;
    }
    Some(LiveReading {
        provider: crate::registry::Provider::Codex,
        account_id,
        at,
        windows,
        origin: path.to_path_buf(),
    })
}

fn creator_account_id(path: &Path) -> Option<String> {
    let mut head = vec![0u8; HEAD_BYTES];
    let mut file = std::fs::File::open(path).ok()?;
    let read = file.read(&mut head).ok()?;
    head.truncate(read);
    let text = String::from_utf8_lossy(&head);
    let first = text.lines().next()?;
    let doc: Value = serde_json::from_str(first).ok()?;
    doc.pointer("/payload/creator_account_id")
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

fn last_rate_limits(path: &Path) -> Option<(u64, Value)> {
    let mut file = std::fs::File::open(path).ok()?;
    let len = file.metadata().ok()?.len();
    let start = len.saturating_sub(TAIL_BYTES);
    file.seek(SeekFrom::Start(start)).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    let text = String::from_utf8_lossy(&tail);
    // The first line of a mid-file tail is usually cut; parsing skips it.
    for line in text.lines().rev() {
        if !line.contains("\"rate_limits\"") {
            continue;
        }
        let Ok(doc) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(limits) = doc
            .pointer("/payload/rate_limits")
            .or_else(|| doc.pointer("/payload/info/rate_limits"))
            .filter(|v| v.is_object())
        else {
            continue;
        };
        let at = doc
            .get("timestamp")
            .and_then(Value::as_str)
            .and_then(parse_utc)?;
        return Some((at, limits.clone()));
    }
    None
}

/// A rollout's `rate_limits` (snake_case) as windows with the same ids the
/// app-server poll uses — `<limit_id>.<slot>` — so a reading refreshes exactly
/// the window it describes. Rollouts report whichever limit the session's
/// model draws on: `codex` for most, `base_model_inference` ("gpt-reserve")
/// for others, which is a different meter and must not overwrite `codex`.
pub fn codex_windows(limits: &Value) -> Vec<Window> {
    let limit_id = limits
        .get("limit_id")
        .and_then(Value::as_str)
        .unwrap_or("codex");
    let name = limits
        .get("limit_name")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .unwrap_or(limit_id);
    let mut windows = Vec::new();
    for slot in ["primary", "secondary"] {
        let Some(window) = limits.get(slot).filter(|w| !w.is_null()) else {
            continue;
        };
        let Some(used) = window.get("used_percent").and_then(Value::as_f64) else {
            continue;
        };
        let minutes = window.get("window_minutes").and_then(Value::as_u64);
        windows.push(Window {
            id: format!("{limit_id}.{slot}"),
            label: format!("{name} {}", crate::codex::duration_label(minutes)),
            used_percent: used,
            resets_at: window.get("resets_at").and_then(Value::as_u64),
            window_minutes: minutes,
            last_reset_at: None,
        });
    }
    windows
}

/// `2026-09-27T11:20:17.972Z` → epoch seconds. Rollouts always write UTC
/// with a `Z`; anything else is refused rather than misread.
fn parse_utc(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.split('.').next()?;
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (t.next()??, t.next()??, t.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    // Days from the civil calendar (Howard Hinnant's algorithm).
    let y = if month <= 2 { year - 1 } else { year };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (month + 9) % 12;
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    let secs = days * 86_400 + hour * 3_600 + minute * 60 + second;
    u64::try_from(secs).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn polled(at: u64, used: f64, resets_at: u64) -> crate::snapshot::AccountSnapshot {
        crate::snapshot::AccountSnapshot {
            captured_at: at,
            account_id: Some("uuid-a".into()),
            windows: vec![
                Window {
                    id: "five_hour".into(),
                    label: "5h".into(),
                    used_percent: 5.0,
                    resets_at: Some(at + 3_000),
                    window_minutes: Some(300),
                    last_reset_at: None,
                },
                Window {
                    id: "seven_day".into(),
                    label: "7d".into(),
                    used_percent: used,
                    resets_at: Some(resets_at),
                    window_minutes: Some(10_080),
                    last_reset_at: Some(at - 100),
                },
            ],
            ..crate::snapshot::AccountSnapshot::empty(
                crate::registry::Provider::Claude,
                "claude-1",
                None,
                "statusline",
            )
        }
    }

    fn reading(at: u64, used: f64, resets_at: u64) -> LiveReading {
        LiveReading {
            provider: crate::registry::Provider::Claude,
            account_id: "uuid-a".into(),
            at,
            windows: vec![Window {
                id: "seven_day".into(),
                label: "7d".into(),
                used_percent: used,
                resets_at: Some(resets_at),
                window_minutes: Some(10_080),
                last_reset_at: None,
            }],
            origin: PathBuf::from("live/claude-uuid-a.json"),
        }
    }

    #[test]
    fn a_live_reading_refreshes_only_the_windows_it_names() {
        let last = polled(1_000, 20.0, 500_000);
        let next = apply(&last, &reading(2_000, 44.0, 500_000)).unwrap();
        assert_eq!(next.captured_at, 2_000);
        assert_eq!(next.source, "live");
        let seven = next.windows.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(seven.used_percent, 44.0);
        assert_eq!(seven.last_reset_at, Some(900), "reset memory is kept");
        let five = next.windows.iter().find(|w| w.id == "five_hour").unwrap();
        assert_eq!(
            five.used_percent, 5.0,
            "untouched window keeps the poll's value"
        );
        assert_eq!(next.window_sources["seven_day"].source, "live");
        assert_eq!(next.window_sources["five_hour"].source, "statusline");
        assert_eq!(next.window_sources["five_hour"].at, 1_000);
    }

    #[test]
    fn a_partial_drop_inside_a_running_window_is_stale_but_a_real_reset_is_kept() {
        let last = polled(1_000, 20.0, 500_000);
        // 20% -> 12% with the same reset ahead: another session's old numbers.
        let next = apply(&last, &reading(2_000, 12.0, 500_000)).unwrap();
        let seven = next.windows.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(seven.used_percent, 20.0);
        assert_eq!(next.window_sources["seven_day"].source, "statusline");
        // 20% -> 1%: that is what an early reset looks like, and it is kept.
        let next = apply(&last, &reading(2_000, 1.0, 500_000)).unwrap();
        let seven = next.windows.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(seven.used_percent, 1.0);
    }

    #[test]
    fn a_reading_with_another_logins_reset_is_refused() {
        let last = polled(1_000, 20.0, 500_000);
        let error = apply(&last, &reading(2_000, 90.0, 900_000)).unwrap_err();
        assert!(error.contains("another login"), "{error}");
    }

    #[test]
    fn a_window_that_already_reset_may_move_on() {
        let last = polled(1_000, 20.0, 1_500);
        let next = apply(&last, &reading(2_000, 1.0, 606_000)).unwrap();
        let seven = next.windows.iter().find(|w| w.id == "seven_day").unwrap();
        assert_eq!(seven.used_percent, 1.0);
    }

    #[test]
    fn only_a_newer_reading_for_the_same_login_is_picked() {
        let last = polled(1_000, 20.0, 500_000);
        let mut other = reading(3_000, 50.0, 500_000);
        other.account_id = "uuid-b".into();
        let readings = vec![
            reading(900, 10.0, 500_000),
            reading(2_000, 30.0, 500_000),
            other,
        ];
        let claude = crate::registry::Provider::Claude;
        let newest = newest_for(&readings, claude, "uuid-a", last.captured_at);
        assert_eq!(newest.map(|r| r.at), Some(2_000));
        assert!(
            newest_for(&readings, claude, "uuid-a", 2_000).is_none(),
            "nothing newer"
        );
        let codex = crate::registry::Provider::Codex;
        assert!(
            newest_for(&readings, codex, "uuid-a", 0).is_none(),
            "other product"
        );
    }

    #[test]
    fn utc_timestamps_parse_like_the_rollouts_write_them() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-09-27T11:20:17.972Z"), Some(1_790_508_017));
        assert_eq!(parse_utc("2026-09-27T11:20:17+02:00"), None);
        assert_eq!(parse_utc("2026-13-01T00:00:00Z"), None);
    }

    fn write_rollout(dir: &Path, creator: Option<&str>, events: &[(&str, Value)]) -> PathBuf {
        let day = dir.join("sessions/2026/09/27");
        std::fs::create_dir_all(&day).unwrap();
        let path = day.join(format!("rollout-{}.jsonl", events.len()));
        let mut meta =
            serde_json::json!({"type": "session_meta", "payload": {"cli_version": "0.158.0"}});
        if let Some(creator) = creator {
            meta["payload"]["creator_account_id"] = Value::from(creator);
        }
        let mut text = format!("{meta}\n");
        for (at, limits) in events {
            text.push_str(&format!(
                "{}\n",
                serde_json::json!({"timestamp": at, "type": "event_msg", "payload": {"type": "token_count", "rate_limits": limits}})
            ));
        }
        std::fs::write(&path, text).unwrap();
        path
    }

    fn codex_limits(used: f64) -> Value {
        serde_json::json!({
            "limit_id": "codex", "limit_name": null,
            "primary": {"used_percent": used, "window_minutes": 10080, "resets_at": 1790898963},
            "secondary": null
        })
    }

    #[test]
    fn a_rollout_gives_its_login_and_its_latest_reading() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            temp.path(),
            Some("a3948a24"),
            &[
                ("2026-09-27T10:00:00Z", codex_limits(10.0)),
                ("2026-09-27T11:00:00Z", codex_limits(44.0)),
            ],
        );
        let reading = read_rollout(&path).expect("reading");
        assert_eq!(reading.account_id, "a3948a24");
        assert_eq!(reading.windows.len(), 1);
        assert_eq!(reading.windows[0].id, "codex.primary");
        assert_eq!(reading.windows[0].used_percent, 44.0);
        assert_eq!(reading.at, parse_utc("2026-09-27T11:00:00Z").unwrap());
    }

    #[test]
    fn rollouts_not_written_since_the_last_merge_are_not_opened() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            temp.path(),
            Some("a3948a24"),
            &[("2026-09-27T11:00:00Z", codex_limits(44.0))],
        );
        let homes = vec![temp.path().to_path_buf()];
        assert_eq!(codex_readings(&homes, 0).len(), 1);
        let written = std::fs::metadata(&path)
            .unwrap()
            .modified()
            .unwrap()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        assert!(codex_readings(&homes, written + 1).is_empty());
    }

    #[test]
    fn a_rollout_without_a_login_is_skipped_not_guessed() {
        let temp = tempfile::tempdir().unwrap();
        let path = write_rollout(
            temp.path(),
            None,
            &[("2026-09-27T10:00:00Z", codex_limits(10.0))],
        );
        assert!(read_rollout(&path).is_none());
    }

    #[test]
    fn a_different_meter_keeps_its_own_window_id() {
        let windows = codex_windows(&serde_json::json!({
            "limit_id": "base_model_inference", "limit_name": "gpt-reserve",
            "primary": {"used_percent": 3.0, "window_minutes": 10080, "resets_at": 1}
        }));
        assert_eq!(windows[0].id, "base_model_inference.primary");
        assert!(windows[0].label.starts_with("gpt-reserve"));
    }

    #[test]
    fn echo_files_become_claude_readings() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::write(
            temp.path().join("claude-uuid-a.json"),
            serde_json::json!({
                "schema": "ovm-limits-live/v1", "provider": "claude", "account_id": "uuid-a",
                "at": 1_790_000_000u64,
                "windows": {"five_hour": {"used_percentage": 12, "resets_at": 1_790_010_000u64},
                            "seven_day": {"used_percentage": 40, "resets_at": 1_790_500_000u64}}
            })
            .to_string(),
        )
        .unwrap();
        std::fs::write(temp.path().join(".login-cache.json"), "{}").unwrap();
        let readings = claude_readings(temp.path());
        assert_eq!(readings.len(), 1);
        assert_eq!(readings[0].account_id, "uuid-a");
        let ids: Vec<&str> = readings[0].windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(ids, ["five_hour", "seven_day"]);
    }

    #[test]
    fn no_codex_account_means_no_rollout_is_read_and_nothing_panics() {
        let temp = tempfile::tempdir().unwrap();
        assert!(codex_readings(&[temp.path().to_path_buf()], u64::MAX).is_empty());
    }
}
