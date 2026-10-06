//! Resets the providers announced, as agentresets.com collects them from the
//! official accounts. The public feed puts them beside the resets this poller
//! saw land, so a reader can line up "announced at" with "arrived at".
//!
//! Fetched by the poller, at most once an hour, into a cache beside
//! `limits.json`; the page never fetches it itself. Fail-open throughout: a
//! fetch that fails or returns nonsense keeps the last good copy, and a poll
//! or a publish never waits on it for longer than one short timeout.

use crate::paths::LimitsDirs;
use crate::registry::Provider;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::process::Command;

pub const URL: &str = "https://agentresets.com/api/resets.json";
/// Overrides [`URL`]; set to an empty string to switch fetching off.
pub const URL_ENV: &str = "OVM_LIMITS_ANNOUNCEMENTS_URL";
pub const NAME: &str = "agentresets.com";
pub const HOME: &str = "https://agentresets.com";
/// How often the source is asked, at most.
const REFRESH_SECONDS: u64 = 3600;
/// The longest a fetch may hold up the poll that asked for it.
const FETCH_TIMEOUT_SECONDS: &str = "10";
/// A summary is a line on a page, not the post.
pub const SUMMARY_CHARS: usize = 140;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Announcement {
    /// The source's id for it (the post's id).
    pub id: String,
    /// `YYYY-MM-DD`, UTC.
    pub date: String,
    /// When it was announced, epoch seconds.
    pub ts: u64,
    pub provider: Provider,
    /// `regular` (applied at once) or `banked` (a reset to spend later).
    pub kind: String,
    /// Who it was for, as announced: `all`, `paid`, `Pro+Max`…
    pub scope: String,
    pub summary: String,
    pub url: String,
    /// When the provider said it had finished rolling it out, if it did.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub delivered_at: Option<u64>,
}

/// Who collected it and whose history it carries, verbatim from the source.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Attribution {
    pub name: String,
    pub url: String,
    pub note: String,
    /// The sites the source credits for its seeded history.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub sources: Vec<Credit>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Credit {
    pub name: String,
    pub url: String,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Announcements {
    /// Oldest first.
    pub items: Vec<Announcement>,
    pub attribution: Attribution,
}

/// Fetch the announcements if the cache is an hour old or more. Never fails:
/// whatever goes wrong, the last good copy stays where it was.
pub fn refresh(dirs: &LimitsDirs, now: u64) {
    let url = std::env::var(URL_ENV).unwrap_or_else(|_| URL.to_string());
    if url.is_empty() {
        return;
    }
    refresh_with(dirs, now, || fetch(&url));
}

/// [`refresh`] with the fetch handed in, for tests.
pub fn refresh_with(dirs: &LimitsDirs, now: u64, fetch: impl FnOnce() -> Option<String>) {
    let checked = std::fs::read_to_string(dirs.announcements_checked_file())
        .ok()
        .and_then(|raw| raw.trim().parse::<u64>().ok());
    if checked.is_some_and(|at| now.saturating_sub(at) < REFRESH_SECONDS) {
        return;
    }
    // Stamped before asking, so a source that is down is asked once an hour
    // like one that is up, not on every poll.
    let _ = crate::snapshot::write_atomic(
        &dirs.announcements_checked_file(),
        format!("{now}\n").as_bytes(),
    );
    let Some(body) = fetch() else {
        return;
    };
    if parse(&body).is_some() {
        let _ = crate::snapshot::write_atomic(&dirs.announcements_file(), body.as_bytes());
    }
}

fn fetch(url: &str) -> Option<String> {
    let agent = format!(
        "ovm-limits/{} (+https://ovm.sh/limits)",
        env!("CARGO_PKG_VERSION")
    );
    let output = Command::new("curl")
        .args([
            "-fsS",
            "--max-time",
            FETCH_TIMEOUT_SECONDS,
            "-A",
            &agent,
            url,
        ])
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    String::from_utf8(output.stdout).ok()
}

/// The cached announcements, or nothing when there is no usable cache.
pub fn load(dirs: &LimitsDirs) -> Option<Announcements> {
    parse(&std::fs::read_to_string(dirs.announcements_file()).ok()?)
}

/// The source's JSON → announcements. `None` when it is not the shape we
/// know, so a changed format reads as "no announcements", never as an error.
pub fn parse(raw: &str) -> Option<Announcements> {
    let doc: Value = serde_json::from_str(raw).ok()?;
    let products = doc.get("products")?.as_object()?;
    let text = |value: &Value, key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let mut items = Vec::new();
    for (product, body) in products {
        let Some(provider) = Provider::parse(product) else {
            continue;
        };
        let Some(events) = body.get("events").and_then(Value::as_array) else {
            continue;
        };
        for event in events {
            if text(event, "type").is_some_and(|t| t != "reset") {
                continue;
            }
            let (Some(id), Some(ts)) = (
                text(event, "id"),
                text(event, "ts").and_then(|t| parse_utc(&t)),
            ) else {
                continue;
            };
            items.push(Announcement {
                date: text(event, "date").unwrap_or_else(|| ts_date(ts)),
                id,
                ts,
                provider,
                kind: text(event, "kind").unwrap_or_else(|| "regular".into()),
                scope: text(event, "scope").unwrap_or_else(|| "all".into()),
                summary: shorten(&text(event, "summary").unwrap_or_default()),
                url: text(event, "url").unwrap_or_default(),
                delivered_at: text(event, "delivered_at").and_then(|t| parse_utc(&t)),
            });
        }
    }
    items.sort_by_key(|a| a.ts);
    let sources = doc
        .get("attribution")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    Some(Credit {
                        name: text(c, "name")?,
                        url: text(c, "url")?,
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    Some(Announcements {
        items,
        attribution: Attribution {
            name: NAME.into(),
            url: HOME.into(),
            note: doc
                .get("note")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            sources,
        },
    })
}

fn shorten(text: &str) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.chars().count() <= SUMMARY_CHARS {
        return text;
    }
    let cut: String = text.chars().take(SUMMARY_CHARS - 1).collect();
    format!("{}…", cut.trim_end())
}

/// `2026-10-04T20:33:43Z` → epoch seconds. UTC only, which is all the
/// source writes.
pub fn parse_utc(text: &str) -> Option<u64> {
    let text = text.strip_suffix('Z')?;
    let (date, time) = text.split_once('T')?;
    let mut date = date.split('-').map(|p| p.parse::<i64>().ok());
    let (year, month, day) = (date.next()??, date.next()??, date.next()??);
    let time = time.split('.').next()?;
    let mut time = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hour, minute, second) = (time.next()??, time.next()??, time.next()??);
    if !(1..=12).contains(&month) || !(1..=31).contains(&day) || hour > 23 || minute > 59 {
        return None;
    }
    let seconds = days_from_civil(year, month, day) * 86_400 + hour * 3600 + minute * 60 + second;
    u64::try_from(seconds).ok()
}

/// Days since 1970-01-01 for a proleptic Gregorian date (Howard Hinnant's
/// `days_from_civil`).
fn days_from_civil(year: i64, month: i64, day: i64) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    let month_index = (month + 9) % 12;
    let day_of_year = (153 * month_index + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

/// Epoch seconds → `YYYY-MM-DD`, for an announcement that left the date out.
fn ts_date(ts: u64) -> String {
    let days = (ts / 86_400) as i64 + 719_468;
    let era = days.div_euclid(146_097);
    let day_of_era = days - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_index = (5 * day_of_year + 2) / 153;
    let day = day_of_year - (153 * month_index + 2) / 5 + 1;
    let month = if month_index < 10 {
        month_index + 3
    } else {
        month_index - 9
    };
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    format!("{year:04}-{month:02}-{day:02}")
}

#[cfg(test)]
pub const FIXTURE: &str = r#"{
  "schema": 1,
  "note": "Codex history before 2026-09-10 seeded from codex-resets.com (https://codex-resets.com) — attribution required.",
  "products": {
    "codex": {"name": "Codex", "events": [
      {"id": "c-1002", "date": "2026-10-02", "ts": "2026-10-02T17:00:00Z", "type": "reset",
       "kind": "regular", "scope": "all", "url": "https://x.com/example/status/1002",
       "summary": "We are resetting usage limits for everyone on Codex. Enjoy the weekend, and thank you for bearing with us through a rough week of capacity problems and slow responses."},
      {"id": "c-0801", "date": "2026-08-01", "ts": "2026-08-01T09:00:00Z", "type": "reset",
       "kind": "banked", "scope": "paid", "url": "https://x.com/example/status/801", "summary": "Old one."}
    ]},
    "claude": {"name": "Claude", "events": [
      {"id": "a-0922", "date": "2026-09-22", "ts": "2026-09-22T16:44:06Z", "type": "reset",
       "scope": "Pro+Max+Team", "url": "https://x.com/example/status/922", "kind": "banked",
       "summary": "A reset to use any time."}
    ]},
    "gemini": {"events": [{"id": "g-1", "ts": "2026-10-01T00:00:00Z", "summary": "not ours"}]}
  },
  "attribution": [{"name": "codex-resets.com", "url": "https://codex-resets.com", "covers": "codex"}]
}"#;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utc_stamps_round_trip_through_the_calendar() {
        assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(parse_utc("2026-10-04T20:33:43Z"), Some(1_791_146_023));
        assert_eq!(parse_utc("2024-02-29T12:00:00.5Z"), Some(1_709_208_000));
        assert_eq!(ts_date(1_791_146_023), "2026-10-04");
        assert_eq!(ts_date(1_709_208_000), "2024-02-29");
        assert_eq!(parse_utc("2026-10-04 20:33:43"), None, "UTC with a Z only");
        assert_eq!(parse_utc("2026-13-04T20:33:43Z"), None);
    }

    #[test]
    fn the_source_reads_into_announcements_with_its_note_verbatim() {
        let parsed = parse(FIXTURE).unwrap();
        let ids: Vec<&str> = parsed.items.iter().map(|a| a.id.as_str()).collect();
        assert_eq!(
            ids,
            ["c-0801", "a-0922", "c-1002"],
            "oldest first, ours only"
        );
        let codex = &parsed.items[2];
        assert_eq!(codex.provider, Provider::Codex);
        assert_eq!(codex.kind, "regular");
        assert_eq!(codex.scope, "all");
        assert!(codex.summary.chars().count() <= SUMMARY_CHARS);
        assert!(codex.summary.ends_with('…'));
        assert_eq!(parsed.items[1].kind, "banked");
        assert!(parsed.attribution.note.starts_with("Codex history before"));
        assert_eq!(parsed.attribution.name, NAME);
        assert_eq!(parsed.attribution.sources[0].name, "codex-resets.com");
        assert!(parse("<html>rate limited</html>").is_none());
        assert!(parse(r#"{"schema":2}"#).is_none());
    }

    #[test]
    fn the_cache_fails_open_and_is_asked_once_an_hour() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        dirs.ensure_layout().unwrap();
        assert!(load(&dirs).is_none(), "no cache is no announcements");

        refresh_with(&dirs, 10_000, || Some(FIXTURE.into()));
        assert_eq!(load(&dirs).unwrap().items.len(), 3);

        let mut asked = false;
        refresh_with(&dirs, 10_000 + 1800, || {
            asked = true;
            None
        });
        assert!(!asked, "within the hour the cache answers");

        refresh_with(&dirs, 10_000 + 3600, || None);
        assert_eq!(
            load(&dirs).unwrap().items.len(),
            3,
            "a failed fetch keeps it"
        );
        refresh_with(&dirs, 10_000 + 7200, || Some("<html>502</html>".into()));
        assert_eq!(load(&dirs).unwrap().items.len(), 3, "so does a bad body");

        let mut asked = false;
        refresh_with(&dirs, 10_000 + 7200 + 60, || {
            asked = true;
            None
        });
        assert!(!asked, "a failure counts as the hour's ask");

        std::fs::write(dirs.announcements_file(), "{not json").unwrap();
        assert!(load(&dirs).is_none(), "a broken cache reads as nothing");
    }
}
