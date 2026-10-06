//! What the person told us about an account's subscription: its name, whether
//! it is still live or has been cancelled, and the day it ends or renews.
//!
//! No product reports any of this through a poll, so it is typed in by hand
//! with `ovm limits plan`, from the line a billing page shows: "Your plan is
//! canceled and won't renew. You'll continue to have access until Oct 30".
//! It lives on the account in the registry and is never sent anywhere.

use crate::{LimitsError, Result};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::fmt;

/// The longest plan name that still sits in a table note.
const MAX_NAME_CHARS: usize = 60;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PlanStatus {
    /// Paid up and renewing.
    #[default]
    Live,
    /// Cancelled: access continues until the end date, then stops.
    Cancelled,
}

impl PlanStatus {
    /// `live` or `cancelled`; the American `canceled` too, since that is how
    /// the billing pages spell it.
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "live" | "active" => Some(Self::Live),
            "cancelled" | "canceled" => Some(Self::Cancelled),
            _ => None,
        }
    }
}

impl fmt::Display for PlanStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Live => f.write_str("live"),
            Self::Cancelled => f.write_str("cancelled"),
        }
    }
}

/// A calendar day, with no time and no zone: the day a billing page names.
/// Written as `YYYY-MM-DD`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct PlanDate {
    pub year: i64,
    /// 1 = January.
    pub month: u32,
    pub day: u32,
}

impl PlanDate {
    /// `2026-10-30`, and only a day that exists.
    pub fn parse(text: &str) -> Option<Self> {
        let mut parts = text.trim().split('-');
        let (year, month, day) = (parts.next()?, parts.next()?, parts.next()?);
        if parts.next().is_some() || year.len() != 4 || month.len() != 2 || day.len() != 2 {
            return None;
        }
        let date = Self {
            year: year.parse().ok()?,
            month: month.parse().ok()?,
            day: day.parse().ok()?,
        };
        let valid = (1..=12).contains(&date.month)
            && date.day >= 1
            && date.day <= days_in_month(date.year, date.month);
        valid.then_some(date)
    }

    /// Days since 1970-01-01. Howard Hinnant's `days_from_civil`.
    pub fn days(self) -> i64 {
        let month = i64::from(self.month);
        let year = if month <= 2 { self.year - 1 } else { self.year };
        let era = year.div_euclid(400);
        let year_of_era = year - era * 400;
        let shifted_month = (month + 9) % 12;
        let day_of_year = (153 * shifted_month + 2) / 5 + i64::from(self.day) - 1;
        let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
        era * 146_097 + day_of_era - 719_468
    }
}

fn days_in_month(year: i64, month: u32) -> u32 {
    let leap = (year % 4 == 0 && year % 100 != 0) || year % 400 == 0;
    match month {
        2 if leap => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

impl fmt::Display for PlanDate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:04}-{:02}-{:02}", self.year, self.month, self.day)
    }
}

impl Serialize for PlanDate {
    fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for PlanDate {
    fn deserialize<D: Deserializer<'de>>(deserializer: D) -> std::result::Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text)
            .ok_or_else(|| serde::de::Error::custom(format!("`{text}` is not a YYYY-MM-DD date")))
    }
}

/// An account's subscription, as the person recorded it.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct Plan {
    /// `ChatGPT Pro 100`, as the billing page names it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub status: PlanStatus,
    /// The last day of access for a cancelled plan; the renewal day for a
    /// live one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ends_on: Option<PlanDate>,
}

/// What `ovm limits plan <account> …` asked for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PlanEdit {
    /// No flags: print the record.
    Show,
    /// `--clear`: forget it.
    Clear,
    /// Any of `--name`, `--status`, `--ends`: change those fields and keep
    /// the rest.
    Set {
        name: Option<String>,
        status: Option<PlanStatus>,
        ends_on: Option<PlanDate>,
    },
}

pub const PLAN_USAGE: &str = "plan <account> [--name \"<plan name>\"] [--status live|cancelled] [--ends YYYY-MM-DD] [--clear]";

impl PlanEdit {
    /// The flags after the account. Each may be given once; `--clear` stands
    /// alone.
    pub fn parse(args: &[String]) -> Result<Self> {
        let mut name = None;
        let mut status = None;
        let mut ends_on = None;
        let mut clear = false;
        let mut rest = args.iter();
        while let Some(flag) = rest.next() {
            match flag.as_str() {
                "--clear" => clear = true,
                "--name" => {
                    let value = flag_value(flag, rest.next())?;
                    validate_name(value)?;
                    name = Some(value.trim().to_string());
                }
                "--status" => {
                    let value = flag_value(flag, rest.next())?;
                    let parsed = PlanStatus::parse(value).ok_or_else(|| {
                        LimitsError::Message(format!(
                            "--status is live or cancelled (not `{value}`)"
                        ))
                    })?;
                    status = Some(parsed);
                }
                "--ends" => {
                    let value = flag_value(flag, rest.next())?;
                    let parsed = PlanDate::parse(value).ok_or_else(|| {
                        LimitsError::Message(format!(
                            "--ends takes a date as YYYY-MM-DD, like 2026-10-30 (not `{value}`)"
                        ))
                    })?;
                    ends_on = Some(parsed);
                }
                other => {
                    return Err(LimitsError::Message(format!(
                        "unknown flag `{other}` — usage: {PLAN_USAGE}"
                    )))
                }
            }
        }
        let setting = name.is_some() || status.is_some() || ends_on.is_some();
        if clear && setting {
            return Err(LimitsError::Message(
                "--clear forgets the whole plan; give it alone".into(),
            ));
        }
        if clear {
            return Ok(Self::Clear);
        }
        if !setting {
            return Ok(Self::Show);
        }
        Ok(Self::Set {
            name,
            status,
            ends_on,
        })
    }

    /// The record after this edit, from the one before it. `Show` keeps it.
    pub fn apply(self, current: Option<Plan>) -> Option<Plan> {
        match self {
            Self::Show => current,
            Self::Clear => None,
            Self::Set {
                name,
                status,
                ends_on,
            } => {
                let mut plan = current.unwrap_or_default();
                if name.is_some() {
                    plan.name = name;
                }
                if let Some(status) = status {
                    plan.status = status;
                }
                if ends_on.is_some() {
                    plan.ends_on = ends_on;
                }
                Some(plan)
            }
        }
    }
}

fn flag_value<'a>(flag: &str, value: Option<&'a String>) -> Result<&'a str> {
    value
        .map(String::as_str)
        .filter(|value| !value.starts_with("--"))
        .ok_or_else(|| LimitsError::Message(format!("{flag} needs a value — usage: {PLAN_USAGE}")))
}

fn validate_name(name: &str) -> Result<()> {
    let trimmed = name.trim();
    let ok = !trimmed.is_empty()
        && trimmed.chars().count() <= MAX_NAME_CHARS
        && !trimmed.chars().any(char::is_control);
    if ok {
        Ok(())
    } else {
        Err(LimitsError::Message(format!(
            "plan name `{name}` must be 1–{MAX_NAME_CHARS} printable characters on one line"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|arg| arg.to_string()).collect()
    }

    fn date(text: &str) -> PlanDate {
        PlanDate::parse(text).unwrap()
    }

    #[test]
    fn dates_parse_only_as_real_days_written_in_full() {
        assert_eq!(
            date("2026-10-30"),
            PlanDate {
                year: 2026,
                month: 10,
                day: 30
            }
        );
        assert_eq!(date("2028-02-29").day, 29, "a leap day");
        for bad in [
            "2026-02-29",
            "2026-13-01",
            "2026-04-31",
            "2026-10-00",
            "2026-1-30",
            "30-10-2026",
            "Oct 30, 2026",
            "2026-10-30-01",
            "",
        ] {
            assert_eq!(PlanDate::parse(bad), None, "{bad}");
        }
        assert_eq!(date("2026-10-30").to_string(), "2026-10-30");
    }

    #[test]
    fn a_date_counts_days_from_the_epoch() {
        assert_eq!(date("1970-01-01").days(), 0);
        assert_eq!(date("2000-03-01").days(), 11_017);
        // Fri 02 Oct 2026, the day of the table tests' clock.
        assert_eq!(date("2026-10-02").days(), 1_790_947_560 / 86_400);
        assert_eq!(date("2026-10-30").days() - date("2026-10-02").days(), 28);
    }

    #[test]
    fn statuses_parse_in_either_spelling() {
        assert_eq!(PlanStatus::parse("live"), Some(PlanStatus::Live));
        assert_eq!(PlanStatus::parse("Cancelled"), Some(PlanStatus::Cancelled));
        assert_eq!(PlanStatus::parse("canceled"), Some(PlanStatus::Cancelled));
        assert_eq!(PlanStatus::parse("paused"), None);
    }

    #[test]
    fn flags_parse_into_an_edit() {
        assert_eq!(PlanEdit::parse(&[]).unwrap(), PlanEdit::Show);
        assert_eq!(
            PlanEdit::parse(&args(&["--clear"])).unwrap(),
            PlanEdit::Clear
        );
        assert_eq!(
            PlanEdit::parse(&args(&[
                "--name",
                "Example Pro 100",
                "--status",
                "cancelled",
                "--ends",
                "2026-10-30"
            ]))
            .unwrap(),
            PlanEdit::Set {
                name: Some("Example Pro 100".into()),
                status: Some(PlanStatus::Cancelled),
                ends_on: Some(date("2026-10-30")),
            }
        );
        assert_eq!(
            PlanEdit::parse(&args(&["--ends", "2026-11-01"])).unwrap(),
            PlanEdit::Set {
                name: None,
                status: None,
                ends_on: Some(date("2026-11-01")),
            }
        );
    }

    #[test]
    fn bad_flags_say_what_was_wrong() {
        let error = |list: &[&str]| PlanEdit::parse(&args(list)).unwrap_err().to_string();
        assert!(error(&["--ends", "Oct 30"]).contains("YYYY-MM-DD"));
        assert!(error(&["--ends", "2026-02-30"]).contains("YYYY-MM-DD"));
        assert!(error(&["--status", "paused"]).contains("live or cancelled"));
        assert!(error(&["--ends"]).contains("needs a value"));
        assert!(error(&["--name", "--clear"]).contains("needs a value"));
        assert!(error(&["--clear", "--status", "live"]).contains("alone"));
        assert!(error(&["--renew"]).contains("unknown flag"));
        assert!(error(&["--name", "  "]).contains("printable"));
    }

    #[test]
    fn an_edit_changes_only_the_fields_it_names() {
        let cancelled = Plan {
            name: Some("Example Pro 100".into()),
            status: PlanStatus::Cancelled,
            ends_on: Some(date("2026-10-30")),
        };
        let renewed = PlanEdit::Set {
            name: None,
            status: Some(PlanStatus::Live),
            ends_on: Some(date("2026-11-30")),
        }
        .apply(Some(cancelled.clone()))
        .unwrap();
        assert_eq!(renewed.name.as_deref(), Some("Example Pro 100"));
        assert_eq!(renewed.status, PlanStatus::Live);
        assert_eq!(renewed.ends_on, Some(date("2026-11-30")));

        let fresh = PlanEdit::Set {
            name: None,
            status: None,
            ends_on: Some(date("2026-10-30")),
        }
        .apply(None)
        .unwrap();
        assert_eq!(fresh.status, PlanStatus::Live, "a new record is live");
        assert_eq!(PlanEdit::Clear.apply(Some(cancelled.clone())), None);
        assert_eq!(
            PlanEdit::Show.apply(Some(cancelled.clone())),
            Some(cancelled)
        );
    }

    #[test]
    fn a_plan_is_written_as_a_small_object() {
        let plan = Plan {
            name: Some("Example Pro 100".into()),
            status: PlanStatus::Cancelled,
            ends_on: Some(date("2026-10-30")),
        };
        let json = serde_json::to_value(&plan).unwrap();
        assert_eq!(
            json,
            serde_json::json!({
                "name": "Example Pro 100",
                "status": "cancelled",
                "ends_on": "2026-10-30"
            })
        );
        assert_eq!(serde_json::from_value::<Plan>(json).unwrap(), plan);
        let bare: Plan = serde_json::from_str("{}").unwrap();
        assert_eq!(bare, Plan::default());
        assert_eq!(
            serde_json::to_string(&bare).unwrap(),
            r#"{"status":"live"}"#
        );
        assert!(serde_json::from_str::<Plan>(r#"{"ends_on": "Oct 30"}"#).is_err());
    }
}
