//! The account table every surface draws the same way: the registry screen,
//! `ovm limits show`, and the poll narration.
//!
//! One flat list, one row per account: name, provider, how much of the
//! account's primary window is left, when it resets back (a clock time and a
//! countdown), and a note. Rows sort by provider, then by the soonest reset,
//! so the account that comes back first sits at the top of its provider; or,
//! with [`SortOrder::Reset`], by the soonest reset alone, whatever the
//! provider. [`Arrangement::grouped`] draws one block per provider instead,
//! each under a header naming it.
//!
//! ```text
//!                        left   resets back
//!   mochi      claude    92%   Mon 02:00        in 2d 12h
//!   bus-s-1    claude    61%   today 18:20      in 4h 54m    team plan · 5h limit only…
//!   lemon      codex      3%   tomorrow 10:51   in 21h       ↺ 1 reset available
//! ```
//!
//! The primary window is the longest one in the account's main pool: 7d for
//! Claude, Codex's own `codex 1w`. Everything else (Claude's 5h, a Codex
//! reserve pool) is a secondary window, shown only when asked for.

use crate::live::CODEX_MAIN_LIMIT;
use crate::plan::{Plan, PlanDate, PlanStatus};
use crate::registry::Provider;
use crate::snapshot::{AccountSnapshot, Window};
use console::style;
use ovm_tui::fixed_width_cell;
use serde::{Deserialize, Serialize};
use std::cmp::Ordering;
use std::fmt;

/// Which rows come first. Persisted in the registry as `table_sort`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SortOrder {
    /// Claude, then Codex; within each, the soonest primary reset first.
    #[default]
    Provider,
    /// The soonest primary reset first, across every provider.
    Reset,
}

impl SortOrder {
    pub fn parse(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "provider" => Some(Self::Provider),
            "reset" => Some(Self::Reset),
            _ => None,
        }
    }

    /// provider → reset → provider.
    pub fn next(self) -> Self {
        match self {
            Self::Provider => Self::Reset,
            Self::Reset => Self::Provider,
        }
    }
}

impl fmt::Display for SortOrder {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Provider => f.write_str("provider"),
            Self::Reset => f.write_str("reset"),
        }
    }
}

/// How the rows are laid out: their order, and whether each provider gets a
/// block of its own.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Arrangement {
    pub sort: SortOrder,
    /// One block per provider, in provider order, each under a header.
    pub grouped: bool,
}

/// What the row says after the name and provider.
pub enum Cells<'a> {
    /// A polled account: left, reset, countdown, note.
    Usage(&'a AccountSnapshot),
    /// The poll failed; the error stands in for the numbers.
    Error(&'a str),
    /// A dim line of state instead of numbers: not signed in, never polled.
    Message(&'a str),
}

/// One account as the table draws it.
pub struct Line<'a> {
    /// The label, or the id when there is no label.
    pub name: &'a str,
    pub provider: Provider,
    pub cells: Cells<'a>,
    /// The plan the person recorded, for the note: when it ends or renews.
    pub plan: Option<&'a Plan>,
}

impl Line<'_> {
    fn snapshot(&self) -> Option<&AccountSnapshot> {
        match self.cells {
            Cells::Usage(snapshot) => Some(snapshot),
            _ => None,
        }
    }
}

/// Column widths. Fitted to the rows for a table; fixed for the narration,
/// which prints one row at a time and has no table to fit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Layout {
    pub name: usize,
    pub provider: usize,
    pub clock: usize,
    pub countdown: usize,
}

const GAP: &str = "   ";
const MIN_NAME_WIDTH: usize = 8;
const MAX_NAME_WIDTH: usize = 20;
/// `100%`.
const LEFT_WIDTH: usize = 4;
/// The clock cell of a short window nobody has used yet.
const NOT_STARTED: &str = "not started";
/// `tomorrow 10:51`, the longest clock inside a week.
const NARRATION_CLOCK_WIDTH: usize = 14;
/// `in 23h 59m`, the longest countdown under a day.
const NARRATION_COUNTDOWN_WIDTH: usize = 10;
/// The note or message cell never shrinks below this, however narrow the
/// terminal: a cut word is worse than a wrapped line.
const MIN_TAIL_WIDTH: usize = 12;
/// At or under this much left, the number is red.
const LEFT_RED_AT: i64 = 10;
/// A plan ending or renewing within this many days is yellow in the note.
const PLAN_SOON_DAYS: i64 = 7;
/// At or under this much left, the number is yellow.
const LEFT_YELLOW_AT: i64 = 30;

const MINUTE: u64 = 60;
const HOUR_MINUTES: u64 = 60;
const DAY_MINUTES: u64 = 24 * HOUR_MINUTES;
const WEEK_MINUTES: u64 = 7 * DAY_MINUTES;
const DAY_SECONDS: i64 = 86_400;
/// Closer than a week, a weekday names the day unambiguously.
const WEEK_SECONDS: u64 = 7 * 86_400;

impl Layout {
    /// Wide enough for every row that is drawn, so the columns line up.
    /// Hidden secondary windows do not widen anything.
    pub fn fit(lines: &[Line], now: u64, secondary: bool) -> Self {
        let name = lines
            .iter()
            .map(|line| console::measure_text_width(line.name))
            .max()
            .unwrap_or(0)
            .clamp(MIN_NAME_WIDTH, MAX_NAME_WIDTH);
        let provider = lines
            .iter()
            .map(|line| line.provider.to_string().chars().count())
            .max()
            .unwrap_or(0);
        let mut clock_width = 0;
        let mut countdown_width = 0;
        for line in lines {
            let Some(snapshot) = line.snapshot() else {
                continue;
            };
            let mut drawn: Vec<&Window> = primary_window(snapshot).into_iter().collect();
            if secondary {
                drawn.extend(secondary_windows(snapshot));
            }
            for window in drawn {
                let (clock_text, countdown_text) = reset_cells(window, now);
                clock_width = clock_width.max(clock_text.chars().count());
                countdown_width = countdown_width.max(countdown_text.chars().count());
            }
        }
        Self {
            name,
            provider,
            clock: clock_width,
            countdown: countdown_width,
        }
    }

    /// Fixed widths for the poll narration, which prints a row as each poll
    /// lands. `name` and `provider` come from the registry so the rows still
    /// line up with each other.
    pub fn narration(name: usize, provider: usize) -> Self {
        Self {
            name: name.clamp(MIN_NAME_WIDTH, MAX_NAME_WIDTH),
            provider,
            clock: NARRATION_CLOCK_WIDTH,
            countdown: NARRATION_COUNTDOWN_WIDTH,
        }
    }

    /// Columns before the left number: name, gap, provider, gap.
    fn lead(&self) -> usize {
        self.name + GAP.len() + self.provider + GAP.len()
    }

    /// Columns before the note.
    fn before_note(&self) -> usize {
        self.lead() + LEFT_WIDTH + GAP.len() + self.clock + GAP.len() + self.countdown + GAP.len()
    }
}

/// Everything a table needs to draw itself.
pub struct Options {
    pub now: u64,
    /// Terminal width; the trailing cell is cut to fit it.
    pub width: usize,
    /// Show each account's secondary windows on dim lines beneath it.
    pub secondary: bool,
    /// Which row carries the `›` marker.
    pub cursor: Option<usize>,
    /// A header line and a block per provider instead of one caption.
    pub grouped: bool,
}

/// Caption and rows, each with its two-column margin. `lines` are drawn in
/// the order given; sort them with [`order`] first. Grouped, a new block
/// starts wherever the provider changes: a blank line, then the provider's
/// header in place of the caption. The columns line up across blocks.
pub fn render(lines: &[Line], options: &Options) -> Vec<String> {
    let layout = Layout::fit(lines, options.now, options.secondary);
    let mut out = Vec::new();
    if !options.grouped {
        out.push(caption(&layout));
    }
    let mut block: Option<Provider> = None;
    for (index, line) in lines.iter().enumerate() {
        if options.grouped && block != Some(line.provider) {
            if block.is_some() {
                out.push(String::new());
            }
            out.push(group_header(line.provider, &layout));
            block = Some(line.provider);
        }
        let marker = if options.cursor == Some(index) {
            style("›").cyan().bold().to_string()
        } else {
            " ".to_string()
        };
        out.push(format!(
            "{marker} {}",
            row(line, &layout, options.now, options.width)
        ));
        if !options.secondary {
            continue;
        }
        if let Some(snapshot) = line.snapshot() {
            for window in secondary_windows(snapshot) {
                out.push(format!("  {}", secondary_row(window, &layout, options.now)));
            }
        }
    }
    out
}

/// `left   resets back`, dim, over the columns it names.
pub fn caption(layout: &Layout) -> String {
    let text = format!(
        "{}{:>LEFT_WIDTH$}{GAP}resets back",
        " ".repeat(layout.lead()),
        "left"
    );
    format!("  {}", style(text).dim())
}

/// `Claude Code          left   resets back`, dim: a block's caption, with
/// the provider's name where the name column starts.
pub fn group_header(provider: Provider, layout: &Layout) -> String {
    let lead = layout.lead();
    let text = format!(
        "{:<lead$}{:>LEFT_WIDTH$}{GAP}resets back",
        provider.display_name(),
        "left"
    );
    format!("  {}", style(text).dim())
}

/// One account, without the margin. `width` is the whole terminal line.
pub fn row(line: &Line, layout: &Layout, now: u64, width: usize) -> String {
    let name = style(fixed_width_cell(line.name, layout.name)).bold();
    let provider = style(fixed_width_cell(
        &line.provider.to_string(),
        layout.provider,
    ))
    .dim();
    let lead = format!("{name}{GAP}{provider}{GAP}");
    let margin = 2;
    let text = match &line.cells {
        Cells::Error(error) => {
            let tail = tail_width(width, margin + layout.lead());
            let cell = console::truncate_str(&format!("✗ {error}"), tail, "…").into_owned();
            style(cell).red().to_string()
        }
        Cells::Message(message) => {
            let tail = tail_width(width, margin + layout.lead());
            let cell = console::truncate_str(message, tail, "…").into_owned();
            style(cell).dim().to_string()
        }
        Cells::Usage(snapshot) => match primary_window(snapshot) {
            None => style("no windows reported").dim().to_string(),
            Some(window) => {
                let numbers = window_cells(window, layout, now, true);
                let mut notes = Vec::new();
                let usage = note(snapshot, window);
                if !usage.is_empty() {
                    notes.push(style(usage).dim().to_string());
                }
                let plan = line.plan.and_then(|plan| plan_note(plan, local_day(now)));
                if let Some((text, urgency)) = plan {
                    notes.push(paint(text, urgency));
                }
                if notes.is_empty() {
                    numbers
                } else {
                    let tail = tail_width(width, margin + layout.before_note());
                    let joined = notes.join(&style(" · ").dim().to_string());
                    let note = console::truncate_str(&joined, tail, "…").into_owned();
                    format!("{numbers}{GAP}{note}")
                }
            }
        },
    };
    format!("{lead}{text}").trim_end().to_string()
}

/// A secondary window under its account: the label indented in the name
/// column, the same numbers, all dim. Without the margin.
pub fn secondary_row(window: &Window, layout: &Layout, now: u64) -> String {
    let label = fixed_width_cell(&format!("  {}", window.label), layout.name);
    let blank = " ".repeat(layout.provider);
    let numbers = window_cells(window, layout, now, false);
    let text = format!("{label}{GAP}{blank}{GAP}{numbers}");
    style(text.trim_end()).dim().to_string()
}

fn tail_width(width: usize, used: usize) -> usize {
    width.saturating_sub(used).max(MIN_TAIL_WIDTH)
}

/// `92%   Mon 02:00   in 2d 12h`. `coloured` puts the warning colour on the
/// left number; a dim secondary line keeps its own.
fn window_cells(window: &Window, layout: &Layout, now: u64, coloured: bool) -> String {
    let left = left_percent(window);
    let left_text = format!("{:>LEFT_WIDTH$}", format!("{left}%"));
    let left_text = if coloured {
        left_colour(left, left_text)
    } else {
        left_text
    };
    let (clock_text, countdown_text) = reset_cells(window, now);
    format!(
        "{left_text}{GAP}{}{GAP}{}",
        fixed_width_cell(&clock_text, layout.clock),
        fixed_width_cell(&countdown_text, layout.countdown)
    )
}

/// 100 minus used, rounded, never outside 0–100.
pub fn left_percent(window: &Window) -> i64 {
    100 - window.used_percent.clamp(0.0, 100.0).round() as i64
}

/// Red when almost nothing is left, yellow when it is getting low.
pub fn left_colour(left: i64, text: String) -> String {
    if left <= LEFT_RED_AT {
        style(text).red().to_string()
    } else if left <= LEFT_YELLOW_AT {
        style(text).yellow().to_string()
    } else {
        text
    }
}

/// The clock and the countdown, or what to say when there is none.
fn reset_cells(window: &Window, now: u64) -> (String, String) {
    if not_started(window) {
        return (NOT_STARTED.to_string(), String::new());
    }
    match window.resets_at {
        Some(at) if at > now => (clock(at, now), countdown(at - now)),
        Some(at) => (clock(at, now), "rolled over".to_string()),
        None => ("—".to_string(), String::new()),
    }
}

/// `↺ 2 resets available`, else `team plan · 5h limit only, no weekly limit` for a team seat
/// that only has a short window, else `5h window only`, else nothing.
fn note(snapshot: &AccountSnapshot, primary: &Window) -> String {
    match snapshot.reset_credits_available {
        Some(1) => return "↺ 1 reset available".to_string(),
        Some(count) if count > 1 => return format!("↺ {count} resets available"),
        _ => {}
    }
    if is_short(primary) {
        if snapshot.kind.as_deref() == Some("team") {
            return "team plan · 5h limit only, no weekly limit".to_string();
        }
        return format!("{} window only", primary.label);
    }
    String::new()
}

/// A short window nothing has been used in. Claude's 5h starts on the first
/// message, so until then its reset time is left over from the last one and
/// gives nothing back: there is no countdown worth showing.
fn not_started(window: &Window) -> bool {
    is_short(window) && window.used_percent <= 0.0
}

/// How close a recorded plan's date is, which decides its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Urgency {
    /// Further off than [`PLAN_SOON_DAYS`], or no date at all: dim.
    Calm,
    /// Today or within [`PLAN_SOON_DAYS`]: yellow.
    Soon,
    /// The day has gone: red.
    Passed,
}

/// The plan's part of the note: `ends 30 Oct` for a cancelled plan,
/// `ended 30 Sep` once that day has gone, `cancelled` with no date,
/// `renews 30 Oct` for a live plan with a date, nothing for a live plan
/// without one. `today` is a day number on the local calendar.
pub fn plan_note(plan: &Plan, today: i64) -> Option<(String, Urgency)> {
    let Some(date) = plan.ends_on else {
        return match plan.status {
            PlanStatus::Cancelled => Some(("cancelled".to_string(), Urgency::Calm)),
            PlanStatus::Live => None,
        };
    };
    let days_left = date.days() - today;
    let passed = days_left < 0;
    let verb = match (plan.status, passed) {
        (PlanStatus::Cancelled, false) => "ends",
        (PlanStatus::Cancelled, true) => "ended",
        (PlanStatus::Live, false) => "renews",
        (PlanStatus::Live, true) => "renewed",
    };
    let urgency = if passed {
        Urgency::Passed
    } else if days_left <= PLAN_SOON_DAYS {
        Urgency::Soon
    } else {
        Urgency::Calm
    };
    Some((format!("{verb} {}", day_and_month(date)), urgency))
}

fn paint(text: String, urgency: Urgency) -> String {
    match urgency {
        Urgency::Calm => style(text).dim().to_string(),
        Urgency::Soon => style(text).yellow().to_string(),
        Urgency::Passed => style(text).red().to_string(),
    }
}

/// Shorter than a week: what an account has when its weekly window is
/// missing, like a seat that has only ever reported 5h.
fn is_short(window: &Window) -> bool {
    window
        .window_minutes
        .is_some_and(|minutes| minutes < WEEK_MINUTES)
}

/// The windows that make up the account's own allowance. For Codex that is
/// the `codex.*` meter, not a model's reserve pool; Claude's windows are all
/// one pool. An account whose main meter is missing falls back to all of them.
fn main_pool(snapshot: &AccountSnapshot) -> Vec<&Window> {
    let pool: Vec<&Window> = match snapshot.provider {
        Provider::Codex => snapshot
            .windows
            .iter()
            .filter(|w| w.id.starts_with(CODEX_MAIN_LIMIT))
            .collect(),
        Provider::Claude => snapshot.windows.iter().collect(),
    };
    if pool.is_empty() {
        snapshot.windows.iter().collect()
    } else {
        pool
    }
}

/// The longest window in the main pool; the first listed wins a tie.
pub fn primary_window(snapshot: &AccountSnapshot) -> Option<&Window> {
    let mut best: Option<&Window> = None;
    for window in main_pool(snapshot) {
        let longer = match best {
            None => true,
            Some(current) => {
                window.window_minutes.unwrap_or(0) > current.window_minutes.unwrap_or(0)
            }
        };
        if longer {
            best = Some(window);
        }
    }
    best
}

/// Every window but the primary, longest first.
pub fn secondary_windows(snapshot: &AccountSnapshot) -> Vec<&Window> {
    let primary = primary_window(snapshot);
    let mut rest: Vec<&Window> = snapshot
        .windows
        .iter()
        .filter(|w| !primary.is_some_and(|p| std::ptr::eq(*w, p)))
        .collect();
    rest.sort_by_key(|w| std::cmp::Reverse(w.window_minutes.unwrap_or(0)));
    rest
}

/// The order rows are drawn in. By provider (Claude, then Codex), then
/// whichever account's primary window resets soonest; by reset, the soonest
/// first whatever the provider, a tie going to provider order. Grouped is
/// always by provider first, since each provider is a block. Rows with no
/// reset to sort by go last, of their block or of the table.
pub fn order(
    arrangement: Arrangement,
    a: (Provider, Option<&AccountSnapshot>),
    b: (Provider, Option<&AccountSnapshot>),
) -> Ordering {
    let (a_rank, a_reset) = sort_key(a.0, a.1);
    let (b_rank, b_reset) = sort_key(b.0, b.1);
    let provider_first = arrangement.grouped || arrangement.sort == SortOrder::Provider;
    if provider_first {
        (a_rank, a_reset).cmp(&(b_rank, b_reset))
    } else {
        (a_reset, a_rank).cmp(&(b_reset, b_rank))
    }
}

fn sort_key(provider: Provider, snapshot: Option<&AccountSnapshot>) -> (usize, u64) {
    let rank = Provider::ALL
        .iter()
        .position(|p| *p == provider)
        .unwrap_or(Provider::ALL.len());
    let reset = snapshot
        .filter(|s| s.error.is_none())
        .and_then(primary_window)
        .filter(|w| !not_started(w))
        .and_then(|w| w.resets_at)
        .unwrap_or(u64::MAX);
    (rank, reset)
}

// ── clock and countdown ──────────────────────────────────────────────────

/// `today 18:20`, `tomorrow 10:51`, `Mon 02:00`, `Sat 17 Oct 09:00`, in the
/// machine's own time zone.
pub fn clock(at: u64, now: u64) -> String {
    clock_with_offsets(at, now, utc_offset(at), utc_offset(now))
}

/// [`clock`] with the zone made explicit, so it can be tested anywhere.
fn clock_with_offsets(at: u64, now: u64, at_offset: i64, now_offset: i64) -> String {
    let then = LocalTime::new(at, at_offset);
    let today = LocalTime::new(now, now_offset);
    let time = format!("{:02}:{:02}", then.hour, then.minute);
    let days_ahead = then.day - today.day;
    let within_a_week = at > now && at - now < WEEK_SECONDS;
    match days_ahead {
        0 => format!("today {time}"),
        1 => format!("tomorrow {time}"),
        _ if days_ahead > 1 && within_a_week => format!("{} {time}", WEEKDAYS[then.weekday]),
        _ => format!(
            "{} {:02} {} {time}",
            WEEKDAYS[then.weekday], then.month_day, MONTHS[then.month]
        ),
    }
}

/// `in 4h 54m`, `in 21h`, `in 2d 12h`, `in 6d`. Minutes round up, so a reset
/// is never said to be `in 0m` before it happens; past a day only days and
/// hours are shown.
pub fn countdown(seconds: u64) -> String {
    let minutes = seconds.div_ceil(MINUTE);
    let mut parts = Vec::new();
    if minutes < DAY_MINUTES {
        let hours = minutes / HOUR_MINUTES;
        let rest = minutes % HOUR_MINUTES;
        if hours > 0 {
            parts.push(format!("{hours}h"));
        }
        if rest > 0 || hours == 0 {
            parts.push(format!("{rest}m"));
        }
    } else {
        let days = minutes / DAY_MINUTES;
        let hours = (minutes % DAY_MINUTES) / HOUR_MINUTES;
        parts.push(format!("{days}d"));
        if hours > 0 {
            parts.push(format!("{hours}h"));
        }
    }
    format!("in {}", parts.join(" "))
}

/// Days since 1970-01-01 on the machine's own calendar at that instant.
pub fn local_day(epoch: u64) -> i64 {
    LocalTime::new(epoch, utc_offset(epoch)).day
}

/// `30 Oct`, the way [`clock`] writes a day past this week.
pub fn day_and_month(date: PlanDate) -> String {
    format!("{:02} {}", date.day, month_name(date))
}

/// `Fri 30 Oct 2026`.
pub fn full_date(date: PlanDate) -> String {
    // 1970-01-01 was a Thursday.
    let weekday = (date.days() + 4).rem_euclid(7) as usize;
    format!(
        "{} {} {}",
        WEEKDAYS[weekday],
        day_and_month(date),
        date.year
    )
}

fn month_name(date: PlanDate) -> &'static str {
    MONTHS[(date.month as usize).saturating_sub(1) % MONTHS.len()]
}

const WEEKDAYS: [&str; 7] = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS: [&str; 12] = [
    "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
];

/// A wall-clock reading, from epoch seconds and an offset east of UTC.
struct LocalTime {
    /// Days since 1970-01-01 on the local calendar.
    day: i64,
    /// 0 = Sunday.
    weekday: usize,
    month_day: i64,
    /// 0 = January.
    month: usize,
    hour: i64,
    minute: i64,
}

impl LocalTime {
    fn new(epoch: u64, offset: i64) -> Self {
        let local = epoch as i64 + offset;
        let day = local.div_euclid(DAY_SECONDS);
        let seconds = local.rem_euclid(DAY_SECONDS);
        // 1970-01-01 was a Thursday.
        let weekday = (day + 4).rem_euclid(7) as usize;
        let (month, month_day) = month_and_day(day);
        Self {
            day,
            weekday,
            month_day,
            month,
            hour: seconds / 3_600,
            minute: (seconds % 3_600) / 60,
        }
    }
}

/// Month (0-based) and day of month for a day count since 1970-01-01.
/// Howard Hinnant's `civil_from_days`, years dropped.
fn month_and_day(days: i64) -> (usize, i64) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let day_of_era = z - era * 146_097;
    let year_of_era =
        (day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
    let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * day_of_year + 2) / 153;
    let month_day = day_of_year - (153 * shifted_month + 2) / 5 + 1;
    let month = if shifted_month < 10 {
        shifted_month + 2
    } else {
        shifted_month - 10
    };
    (month as usize, month_day)
}

/// Seconds east of UTC at that instant, daylight saving included.
fn utc_offset(epoch: u64) -> i64 {
    let time = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r writes tm.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return 0;
    }
    tm.tm_gmtoff
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Fri 02 Oct 2026 13:26:00 UTC.
    const NOW: u64 = 1_790_947_560;

    fn window(id: &str, label: &str, used: f64, minutes: u64, resets_at: u64) -> Window {
        Window {
            id: id.into(),
            label: label.into(),
            used_percent: used,
            resets_at: Some(resets_at),
            window_minutes: Some(minutes),
            last_reset_at: None,
        }
    }

    fn snapshot(provider: Provider, id: &str, windows: Vec<Window>) -> AccountSnapshot {
        AccountSnapshot {
            windows,
            ..AccountSnapshot::empty(provider, id, None, "test")
        }
    }

    fn plain(text: &str) -> String {
        console::strip_ansi_codes(text).trim_end().to_string()
    }

    #[test]
    fn the_clock_says_today_tomorrow_weekday_or_date() {
        let at = |days: u64, hours: u64, minutes: u64| {
            NOW + days * 86_400 + hours * 3_600 + minutes * 60
        };
        assert_eq!(clock_with_offsets(at(0, 4, 54), NOW, 0, 0), "today 18:20");
        assert_eq!(
            clock_with_offsets(at(0, 21, 25), NOW, 0, 0),
            "tomorrow 10:51"
        );
        assert_eq!(clock_with_offsets(at(2, 12, 34), NOW, 0, 0), "Mon 02:00");
        assert_eq!(clock_with_offsets(at(6, 0, 34), NOW, 0, 0), "Thu 14:00");
        // Next Friday is still a weekday while it is under a week away…
        assert_eq!(clock_with_offsets(at(6, 18, 37), NOW, 0, 0), "Fri 08:03");
        // …and a date from a week on.
        assert_eq!(
            clock_with_offsets(at(7, 0, 0), NOW, 0, 0),
            "Fri 09 Oct 13:26"
        );
        // The calendar day is the local one: 23:30 UTC is tomorrow in Tokyo.
        let tokyo = 9 * 3_600;
        assert_eq!(
            clock_with_offsets(at(0, 10, 4), NOW, tokyo, tokyo),
            "tomorrow 08:30"
        );
        // Across a year end.
        let new_year = 1_798_761_600; // Fri 01 Jan 2027 00:00 UTC
        assert_eq!(
            clock_with_offsets(new_year + 8 * 86_400, new_year, 0, 0),
            "Sat 09 Jan 00:00"
        );
    }

    #[test]
    fn the_countdown_drops_zero_parts_and_rounds_minutes_up() {
        assert_eq!(countdown(4 * 3_600 + 54 * 60), "in 4h 54m");
        assert_eq!(countdown(4 * 3_600 + 53 * 60 + 1), "in 4h 54m");
        assert_eq!(countdown(21 * 3_600), "in 21h");
        assert_eq!(countdown(30), "in 1m");
        assert_eq!(countdown(45 * 60), "in 45m");
        assert_eq!(countdown(2 * 86_400 + 12 * 3_600 + 59 * 60), "in 2d 12h");
        assert_eq!(countdown(6 * 86_400 + 34 * 60), "in 6d");
        assert_eq!(countdown(86_400 - 10), "in 1d");
    }

    #[test]
    fn left_is_red_at_ten_yellow_at_thirty() {
        console::set_colors_enabled(true);
        let red = left_colour(10, "10%".into());
        let yellow_low = left_colour(11, "11%".into());
        let yellow = left_colour(30, "30%".into());
        let plain_text = left_colour(31, "31%".into());
        assert_eq!(red, style("10%").red().to_string());
        assert_eq!(yellow_low, style("11%").yellow().to_string());
        assert_eq!(yellow, style("30%").yellow().to_string());
        assert_eq!(plain_text, "31%");
        console::set_colors_enabled(false);
    }

    #[test]
    fn the_primary_window_is_the_longest_in_the_main_pool() {
        let claude = snapshot(
            Provider::Claude,
            "claude-1",
            vec![
                window("five_hour", "5h", 4.0, 300, NOW + 3_600),
                window("seven_day", "7d", 7.0, 10_080, NOW + 86_400),
            ],
        );
        assert_eq!(primary_window(&claude).unwrap().label, "7d");
        let secondary: Vec<&str> = secondary_windows(&claude)
            .iter()
            .map(|w| w.label.as_str())
            .collect();
        assert_eq!(secondary, ["5h"]);

        // A reserve pool as long as the main meter is still not the main meter.
        let codex = snapshot(
            Provider::Codex,
            "codex-2",
            vec![
                window(
                    "base_model_inference.primary",
                    "gpt-reserve 1w",
                    0.0,
                    10_080,
                    NOW + 9,
                ),
                window("codex.primary", "codex 1w", 98.0, 10_080, NOW + 99),
            ],
        );
        assert_eq!(primary_window(&codex).unwrap().label, "codex 1w");
    }

    #[test]
    fn rows_sort_by_provider_then_soonest_reset() {
        let late_claude = snapshot(
            Provider::Claude,
            "late",
            vec![window("seven_day", "7d", 1.0, 10_080, NOW + 6 * 86_400)],
        );
        let soon_claude = snapshot(
            Provider::Claude,
            "soon",
            vec![
                window("five_hour", "5h", 1.0, 300, NOW + 60),
                window("seven_day", "7d", 1.0, 10_080, NOW + 2 * 86_400),
            ],
        );
        let short_claude = snapshot(
            Provider::Claude,
            "short",
            vec![window("five_hour", "5h", 5.0, 300, NOW + 3_600)],
        );
        let soon_codex = snapshot(
            Provider::Codex,
            "codex",
            vec![window("codex.primary", "codex 1w", 1.0, 10_080, NOW + 30)],
        );
        let mut rows: Vec<(Provider, Option<&AccountSnapshot>, &str)> = vec![
            (Provider::Codex, Some(&soon_codex), "codex"),
            (Provider::Claude, None, "never"),
            (Provider::Claude, Some(&late_claude), "late"),
            (Provider::Claude, Some(&soon_claude), "soon"),
            (Provider::Claude, Some(&short_claude), "short"),
        ];
        let by_provider = Arrangement::default();
        rows.sort_by(|a, b| order(by_provider, (a.0, a.1), (b.0, b.1)));
        let names: Vec<&str> = rows.iter().map(|r| r.2).collect();
        assert_eq!(names, ["short", "soon", "late", "never", "codex"]);
    }

    #[test]
    fn sorted_by_reset_the_soonest_comes_first_whatever_the_provider() {
        let claude_week = snapshot(
            Provider::Claude,
            "claude-week",
            vec![window("seven_day", "7d", 1.0, 10_080, NOW + 6 * 86_400)],
        );
        let claude_day = snapshot(
            Provider::Claude,
            "claude-day",
            vec![window("seven_day", "7d", 1.0, 10_080, NOW + 86_400)],
        );
        let codex_hour = snapshot(
            Provider::Codex,
            "codex-hour",
            vec![window(
                "codex.primary",
                "codex 1w",
                1.0,
                10_080,
                NOW + 3_600,
            )],
        );
        let codex_days = snapshot(
            Provider::Codex,
            "codex-days",
            vec![window(
                "codex.primary",
                "codex 1w",
                1.0,
                10_080,
                NOW + 3 * 86_400,
            )],
        );
        let mut rows: Vec<(Provider, Option<&AccountSnapshot>, &str)> = vec![
            (Provider::Claude, None, "never"),
            (Provider::Claude, Some(&claude_week), "claude-week"),
            (Provider::Codex, Some(&codex_days), "codex-days"),
            (Provider::Claude, Some(&claude_day), "claude-day"),
            (Provider::Codex, Some(&codex_hour), "codex-hour"),
        ];
        let by_reset = Arrangement {
            sort: SortOrder::Reset,
            grouped: false,
        };
        rows.sort_by(|a, b| order(by_reset, (a.0, a.1), (b.0, b.1)));
        let names: Vec<&str> = rows.iter().map(|r| r.2).collect();
        assert_eq!(
            names,
            [
                "codex-hour",
                "claude-day",
                "codex-days",
                "claude-week",
                "never"
            ]
        );

        // Grouped, each provider is a block, chronological inside it.
        let grouped = Arrangement {
            sort: SortOrder::Reset,
            grouped: true,
        };
        rows.sort_by(|a, b| order(grouped, (a.0, a.1), (b.0, b.1)));
        let names: Vec<&str> = rows.iter().map(|r| r.2).collect();
        assert_eq!(
            names,
            [
                "claude-day",
                "claude-week",
                "never",
                "codex-hour",
                "codex-days"
            ]
        );
    }

    #[test]
    fn sort_orders_parse_print_and_cycle() {
        assert_eq!(SortOrder::parse("provider"), Some(SortOrder::Provider));
        assert_eq!(SortOrder::parse(" Reset "), Some(SortOrder::Reset));
        assert_eq!(SortOrder::parse("name"), None);
        assert_eq!(SortOrder::Provider.to_string(), "provider");
        assert_eq!(SortOrder::default(), SortOrder::Provider);
        assert_eq!(SortOrder::Provider.next(), SortOrder::Reset);
        assert_eq!(SortOrder::Reset.next(), SortOrder::Provider);
    }

    fn table(snapshots: &[(&str, &AccountSnapshot)], secondary: bool) -> Vec<String> {
        table_with(snapshots, secondary, false)
    }

    fn table_with(
        snapshots: &[(&str, &AccountSnapshot)],
        secondary: bool,
        grouped: bool,
    ) -> Vec<String> {
        console::set_colors_enabled(false);
        let lines: Vec<Line> = snapshots
            .iter()
            .map(|(name, snapshot)| Line {
                name,
                provider: snapshot.provider,
                cells: Cells::Usage(snapshot),
                plan: None,
            })
            .collect();
        render(
            &lines,
            &Options {
                now: NOW,
                width: 120,
                secondary,
                cursor: Some(0),
                grouped,
            },
        )
        .iter()
        .map(|line| plain(line))
        .collect()
    }

    #[test]
    fn grouped_each_provider_is_a_block_under_its_own_header() {
        let soon = snapshot(
            Provider::Claude,
            "claude-1",
            vec![window("seven_day", "7d", 8.0, 10_080, NOW + 86_400)],
        );
        let late = snapshot(
            Provider::Claude,
            "claude-2",
            vec![window("seven_day", "7d", 50.0, 10_080, NOW + 3 * 86_400)],
        );
        let lemon = snapshot(
            Provider::Codex,
            "codex-1",
            vec![window(
                "codex.primary",
                "codex 1w",
                97.0,
                10_080,
                NOW + 3_600,
            )],
        );
        let lines = table_with(
            &[("mochi", &soon), ("simcity", &late), ("lemon", &lemon)],
            false,
            true,
        );
        assert_eq!(lines.len(), 6, "{lines:#?}");
        assert_eq!(lines[0], "  Claude Code         left   resets back");
        assert!(
            lines[1].starts_with("› mochi      claude    92%"),
            "{lines:#?}"
        );
        assert!(
            lines[2].starts_with("  simcity    claude    50%"),
            "{lines:#?}"
        );
        assert_eq!(lines[3], "");
        assert_eq!(lines[4], "  Codex               left   resets back");
        assert!(
            lines[5].starts_with("  lemon      codex      3%"),
            "{lines:#?}"
        );

        // Ungrouped, the same rows sit under one caption with no gaps.
        let flat = table(
            &[("mochi", &soon), ("simcity", &late), ("lemon", &lemon)],
            false,
        );
        assert_eq!(flat.len(), 4, "{flat:#?}");
        assert_eq!(flat[0], "                      left   resets back");
        assert!(!flat.iter().any(|line| line.is_empty()), "{flat:#?}");
    }

    #[test]
    fn the_table_has_one_caption_and_one_row_per_account() {
        let bus = snapshot(
            Provider::Claude,
            "claude-3",
            vec![window(
                "five_hour",
                "5h",
                39.0,
                300,
                NOW + 4 * 3_600 + 54 * 60,
            )],
        );
        let mut lemon = snapshot(
            Provider::Codex,
            "codex-2",
            vec![
                window(
                    "base_model_inference.primary",
                    "gpt-reserve 1w",
                    0.0,
                    10_080,
                    NOW + 5 * 86_400,
                ),
                window("codex.primary", "codex 1w", 97.0, 10_080, NOW + 21 * 3_600),
            ],
        );
        lemon.reset_credits_available = Some(1);
        let lines = table(&[("bus-s-1", &bus), ("lemon", &lemon)], false);
        assert_eq!(lines.len(), 3, "{lines:#?}");
        assert_eq!(lines[0], "                      left   resets back");
        // The clock is local; the column is as wide as its widest entry.
        let bus_clock = clock(NOW + 4 * 3_600 + 54 * 60, NOW);
        let lemon_clock = clock(NOW + 21 * 3_600, NOW);
        let width = bus_clock.len().max(lemon_clock.len());
        assert_eq!(
            lines[1],
            format!(
                "› bus-s-1    claude    61%   {bus_clock:<width$}   in 4h 54m   5h window only"
            )
        );
        assert_eq!(
            lines[2],
            format!("  lemon      codex      3%   {lemon_clock:<width$}   in 21h      ↺ 1 reset available")
        );
    }

    #[test]
    fn a_team_seat_with_only_5h_says_it_has_no_weekly_limit() {
        let mut seat = snapshot(
            Provider::Claude,
            "claude-3",
            vec![window("five_hour", "5h", 40.0, 300, NOW + 3_600)],
        );
        seat.kind = Some("team".into());
        let lines = table(&[("seat", &seat)], false);
        assert!(
            lines[1].ends_with("in 1h   team plan · 5h limit only, no weekly limit"),
            "{lines:#?}"
        );
    }

    #[test]
    fn an_unused_5h_window_has_not_started_and_sorts_last() {
        let mut seat = snapshot(
            Provider::Claude,
            "claude-3",
            vec![window("five_hour", "5h", 0.0, 300, NOW + 120)],
        );
        seat.kind = Some("team".into());
        let weekly = snapshot(
            Provider::Claude,
            "claude-2",
            vec![window("seven_day", "7d", 4.0, 10_080, NOW + 6 * 86_400)],
        );
        let by_provider = Arrangement::default();
        let unused = (Provider::Claude, Some(&seat));
        let used = (Provider::Claude, Some(&weekly));
        assert_eq!(order(by_provider, used, unused), Ordering::Less);
        let lines = table(&[("seat", &seat)], false);
        assert!(lines[1].contains("100%   not started"), "{lines:#?}");
        assert!(!lines[1].contains("in 2m"), "{lines:#?}");
        assert!(
            lines[1].ends_with("team plan · 5h limit only, no weekly limit"),
            "{lines:#?}"
        );
    }

    #[test]
    fn several_reset_credits_are_plural_and_zero_says_nothing() {
        let mut codex = snapshot(
            Provider::Codex,
            "codex-1",
            vec![window(
                "codex.primary",
                "codex 1w",
                50.0,
                10_080,
                NOW + 86_400 * 2,
            )],
        );
        codex.reset_credits_available = Some(2);
        let lines = table(&[("codex", &codex)], false);
        assert!(lines[1].ends_with("↺ 2 resets available"), "{lines:#?}");
        codex.reset_credits_available = Some(0);
        let lines = table(&[("codex", &codex)], false);
        assert!(lines[1].ends_with("in 2d"), "{lines:#?}");
    }

    #[test]
    fn secondary_windows_show_only_when_asked() {
        let simcity = snapshot(
            Provider::Claude,
            "claude-1",
            vec![
                window("five_hour", "5h", 4.0, 300, NOW + 3_600),
                window("seven_day", "7d", 7.0, 10_080, NOW + 6 * 86_400),
            ],
        );
        let hidden = table(&[("simcity", &simcity)], false);
        assert_eq!(hidden.len(), 2, "{hidden:#?}");
        let shown = table(&[("simcity", &simcity)], true);
        assert_eq!(shown.len(), 3, "{shown:#?}");
        assert!(shown[1].contains(" 93% "), "{shown:#?}");
        let weekly_clock = clock(NOW + 6 * 86_400, NOW);
        let hourly_clock = clock(NOW + 3_600, NOW);
        let width = weekly_clock.len().max(hourly_clock.len());
        assert_eq!(
            shown[2],
            format!("    5h                 96%   {hourly_clock:<width$}   in 1h")
        );
    }

    #[test]
    fn errors_and_messages_stand_in_for_the_numbers() {
        console::set_colors_enabled(false);
        let lines = [
            Line {
                name: "mochi",
                provider: Provider::Claude,
                cells: Cells::Error("signed out"),
                plan: None,
            },
            Line {
                name: "spare",
                provider: Provider::Codex,
                cells: Cells::Message("never polled — p to poll"),
                plan: None,
            },
        ];
        let out: Vec<String> = render(
            &lines,
            &Options {
                now: NOW,
                width: 80,
                secondary: false,
                cursor: None,
                grouped: false,
            },
        )
        .iter()
        .map(|line| plain(line))
        .collect();
        assert_eq!(out[1], "  mochi      claude   ✗ signed out");
        assert_eq!(out[2], "  spare      codex    never polled — p to poll");
    }

    fn plan(status: PlanStatus, ends_on: Option<&str>) -> Plan {
        Plan {
            name: Some("Example Pro 100".into()),
            status,
            ends_on: ends_on.and_then(PlanDate::parse),
        }
    }

    /// Fri 02 Oct 2026, the day of [`NOW`].
    fn today() -> i64 {
        PlanDate::parse("2026-10-02").unwrap().days()
    }

    #[test]
    fn the_plan_note_says_when_access_ends_or_the_plan_renews() {
        let note = |status, ends_on| plan_note(&plan(status, ends_on), today());
        assert_eq!(note(PlanStatus::Live, None), None, "live, no date");
        assert_eq!(
            note(PlanStatus::Live, Some("2026-10-30")),
            Some(("renews 30 Oct".to_string(), Urgency::Calm))
        );
        assert_eq!(
            note(PlanStatus::Cancelled, None),
            Some(("cancelled".to_string(), Urgency::Calm))
        );
        assert_eq!(
            note(PlanStatus::Cancelled, Some("2026-10-30")),
            Some(("ends 30 Oct".to_string(), Urgency::Calm))
        );
        assert_eq!(
            note(PlanStatus::Cancelled, Some("2026-10-09")),
            Some(("ends 09 Oct".to_string(), Urgency::Soon)),
            "seven days out"
        );
        assert_eq!(
            note(PlanStatus::Cancelled, Some("2026-10-10")),
            Some(("ends 10 Oct".to_string(), Urgency::Calm)),
            "eight days out"
        );
        assert_eq!(
            note(PlanStatus::Cancelled, Some("2026-10-02")),
            Some(("ends 02 Oct".to_string(), Urgency::Soon)),
            "the last day is still a day of access"
        );
        assert_eq!(
            note(PlanStatus::Cancelled, Some("2026-09-30")),
            Some(("ended 30 Sep".to_string(), Urgency::Passed))
        );
        assert_eq!(
            note(PlanStatus::Live, Some("2026-10-05")),
            Some(("renews 05 Oct".to_string(), Urgency::Soon))
        );
    }

    #[test]
    fn the_plan_note_follows_the_reset_credits_and_the_short_window_note() {
        console::set_colors_enabled(false);
        let cancelled = plan(PlanStatus::Cancelled, Some("2026-11-30"));
        let mut credited = snapshot(
            Provider::Codex,
            "lemon",
            vec![window(
                "codex.primary",
                "1w",
                97.0,
                WEEK_MINUTES,
                NOW + 3_600,
            )],
        );
        credited.reset_credits_available = Some(1);
        let short = snapshot(
            Provider::Claude,
            "bus",
            vec![window("five_hour", "5h", 0.0, 300, NOW + 3_600)],
        );
        let plain_weekly = snapshot(
            Provider::Claude,
            "mochi",
            vec![window("seven_day", "7d", 8.0, WEEK_MINUTES, NOW + 86_400)],
        );
        let lines: Vec<Line> = [
            ("lemon", &credited),
            ("bus", &short),
            ("mochi", &plain_weekly),
        ]
        .iter()
        .map(|(name, snapshot)| Line {
            name,
            provider: snapshot.provider,
            cells: Cells::Usage(snapshot),
            plan: Some(&cancelled),
        })
        .collect();
        let options = Options {
            now: NOW,
            width: 160,
            secondary: false,
            cursor: None,
            grouped: false,
        };
        let out: Vec<String> = render(&lines, &options)
            .iter()
            .map(|line| plain(line))
            .collect();
        assert!(
            out[1].ends_with("↺ 1 reset available · ends 30 Nov"),
            "{out:#?}"
        );
        assert!(out[2].ends_with("5h window only · ends 30 Nov"), "{out:#?}");
        assert!(out[3].ends_with("in 1d   ends 30 Nov"), "{out:#?}");
    }
}
