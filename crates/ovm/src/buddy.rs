//! The Claude Code companion record — the buddy hatched by `/buddy`.
//!
//! `/buddy` shipped in Claude Code 2.1.80 and was removed in 2.1.97. What it
//! left behind is a single object in the top-level Claude config:
//!
//! ```json
//! "companion": { "name": "…", "personality": "…", "hatchedAt": 1775033212488 }
//! ```
//!
//! Nothing reads that key any more — the removal took the reader, not the
//! record — so a buddy hatched on 2.1.96 survives untouched on every release
//! since. That is the whole reason the story and the tour can show you *your*
//! cat rather than a picture of someone else's.
//!
//! Rarity and the stat bars (`★ COMMON`, `CHONK`, `DEBUGGING 75`) are NOT in
//! here: 2.1.96 derived them at render time and the deriving code went with the
//! feature. We render what was actually kept and claim nothing more.

use serde::Deserialize;
use std::path::PathBuf;

/// A hatched buddy, as `/buddy` wrote it.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Buddy {
    pub name: String,
    pub personality: String,
    /// Milliseconds since the Unix epoch, as JavaScript wrote it.
    #[serde(rename = "hatchedAt")]
    pub hatched_at_ms: i64,
}

#[derive(Deserialize)]
struct ClaudeConfig {
    #[serde(rename = "companion")]
    buddy: Option<Buddy>,
}

/// The top-level Claude config file.
///
/// `CLAUDE_CONFIG_DIR` relocates this file along with the rest of the Claude
/// home — that is how `ovm claudex` keeps its own session state out of the real
/// one — so honouring it here means the tour reads the same config the launch
/// path would actually write to, not a hardcoded `~/.claude.json`.
fn config_path() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLAUDE_CONFIG_DIR") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join(".claude.json"));
        }
    }
    dirs::home_dir().map(|home| home.join(".claude.json"))
}

impl Buddy {
    /// The buddy on this machine, if one was ever hatched.
    ///
    /// Every failure is `None`: no config, unreadable config, config that isn't
    /// JSON, no `companion` key. A missing buddy is the ordinary case for
    /// anyone who arrived after 2.1.97, not an error worth a message.
    pub fn load() -> Option<Self> {
        Self::load_from(&config_path()?)
    }

    pub fn load_from(path: &std::path::Path) -> Option<Self> {
        let raw = std::fs::read_to_string(path).ok()?;
        serde_json::from_str::<ClaudeConfig>(&raw).ok()?.buddy
    }

    /// The hatch day, spelled the way the story spells everything else:
    /// lowercase, no padding — "1 april 2026".
    pub fn hatched_on(&self) -> String {
        let (year, month, day) = civil_from_unix_ms(self.hatched_at_ms);
        const MONTHS: [&str; 12] = [
            "january",
            "february",
            "march",
            "april",
            "may",
            "june",
            "july",
            "august",
            "september",
            "october",
            "november",
            "december",
        ];
        format!("{day} {} {year}", MONTHS[(month - 1) as usize])
    }
}

/// Milliseconds since the epoch → civil (year, month, day), UTC.
///
/// Hand-rolled rather than pulling in a date crate for one label: this is
/// Howard Hinnant's `civil_from_days`, and the only input it will ever see is a
/// timestamp some other program already wrote.
fn civil_from_unix_ms(ms: i64) -> (i64, u32, u32) {
    // Floor division, so timestamps before 1970 don't round towards zero.
    let days = ms.div_euclid(86_400_000);
    let shifted = days + 719_468;
    let era = if shifted >= 0 {
        shifted
    } else {
        shifted - 146_096
    } / 146_097;
    let day_of_era = shifted - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let year = year_of_era + era * 400;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let month_prime = (5 * day_of_year + 2) / 153;
    let day = (day_of_year - (153 * month_prime + 2) / 5 + 1) as u32;
    let month = if month_prime < 10 {
        month_prime + 3
    } else {
        month_prime - 9
    } as u32;
    (if month <= 2 { year + 1 } else { year }, month, day)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn config_with(body: &str) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let mut file = std::fs::File::create(dir.path().join(".claude.json")).expect("create");
        file.write_all(body.as_bytes()).expect("write");
        dir
    }

    #[test]
    fn reads_the_record_buddy_left_behind() {
        let dir = config_with(
            r#"{ "someOtherKey": 1,
                 "companion": { "name": "Quelpaw",
                                "personality": "Debugging genius.",
                                "hatchedAt": 1775033212488 } }"#,
        );
        let buddy = Buddy::load_from(&dir.path().join(".claude.json")).expect("buddy");
        assert_eq!(buddy.name, "Quelpaw");
        assert_eq!(buddy.personality, "Debugging genius.");
        assert_eq!(buddy.hatched_at_ms, 1775033212488);
    }

    /// The ordinary case for anyone who arrived after 2.1.97 — and the branch
    /// the tour uses to decide whether it may offer to hatch at all. A wrong
    /// `None` here is harmless; a wrong `Some` would suppress the offer.
    #[test]
    fn a_config_without_a_buddy_is_none_not_an_error() {
        let dir = config_with(r#"{ "numStartups": 12 }"#);
        assert!(Buddy::load_from(&dir.path().join(".claude.json")).is_none());
    }

    /// Never hand the caller an error for a config that isn't ours to parse:
    /// the tour must not fail because some unrelated key changed shape.
    #[test]
    fn unreadable_or_malformed_config_is_none() {
        let dir = config_with("not json at all");
        assert!(Buddy::load_from(&dir.path().join(".claude.json")).is_none());
        assert!(Buddy::load_from(&dir.path().join("nothing-here.json")).is_none());
    }

    /// A companion missing a field is not a companion — the story would render
    /// a card with a hole in it.
    #[test]
    fn a_partial_record_is_none() {
        let dir = config_with(r#"{ "companion": { "name": "Quelpaw" } }"#);
        assert!(Buddy::load_from(&dir.path().join(".claude.json")).is_none());
    }

    #[test]
    fn hatch_day_is_april_fools() {
        let buddy = Buddy {
            name: "Quelpaw".into(),
            personality: String::new(),
            // The real record: /buddy was "here for April 1st", and it was.
            hatched_at_ms: 1775033212488,
        };
        assert_eq!(buddy.hatched_on(), "1 april 2026");
    }

    #[test]
    fn civil_dates_span_epochs_and_leap_days() {
        assert_eq!(civil_from_unix_ms(0), (1970, 1, 1));
        assert_eq!(civil_from_unix_ms(-1), (1969, 12, 31));
        // 2024-02-29 — a leap day in a century that is a leap year.
        assert_eq!(civil_from_unix_ms(1_709_164_800_000), (2024, 2, 29));
        // 2000-02-29 — the 400-year rule.
        assert_eq!(civil_from_unix_ms(951_782_400_000), (2000, 2, 29));
    }
}
