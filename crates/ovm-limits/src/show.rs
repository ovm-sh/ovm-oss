//! `ovm limits show` — the merged view as a table, or the file itself.

use crate::paths::LimitsDirs;
use crate::plan::{Plan, PlanStatus};
use crate::registry::Registry;
use crate::snapshot::{self, AccountSnapshot, Merged, Window};
use crate::table::{self, Cells, Line};
use crate::{LimitsError, Result};
use console::style;
use std::fmt::Write as _;

/// Which shape of the merged view to print.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    /// The account table, one row each — the same one the registry screen
    /// draws.
    Table,
    /// The whole merged file, every field.
    Json,
    /// Plain lines for a push notification or anything else that is not a
    /// terminal. What the digest hook sends.
    Brief,
    /// Every account on one screen: grouped by provider, one row per account,
    /// one column per window.
    Grid,
}

/// `--sort provider|reset`, `--group`, `--no-group`: a change to the
/// persisted arrangement for one call. Only the table reads them; `--json`,
/// `--brief` and `--grid` print what they always print.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Overrides {
    pub sort: Option<table::SortOrder>,
    pub grouped: Option<bool>,
}

impl Overrides {
    /// The arrangement flags taken out of `args`, and every other argument
    /// in its order.
    pub fn take(args: &[String]) -> Result<(Self, Vec<String>)> {
        let mut overrides = Self::default();
        let mut rest = Vec::new();
        let mut iter = args.iter();
        while let Some(arg) = iter.next() {
            match arg.as_str() {
                "--sort" => {
                    let value = iter.next().ok_or_else(sort_usage)?;
                    overrides.sort = Some(table::SortOrder::parse(value).ok_or_else(sort_usage)?);
                }
                "--group" => overrides.grouped = Some(true),
                "--no-group" => overrides.grouped = Some(false),
                _ => rest.push(arg.clone()),
            }
        }
        Ok((overrides, rest))
    }

    /// A flag wins over the setting; no flag leaves the setting as it is.
    pub fn apply(&self, persisted: table::Arrangement) -> table::Arrangement {
        table::Arrangement {
            sort: self.sort.unwrap_or(persisted.sort),
            grouped: self.grouped.unwrap_or(persisted.grouped),
        }
    }
}

fn sort_usage() -> LimitsError {
    LimitsError::Message("usage: show --sort provider|reset".into())
}

/// A merge with no accounts in it means what no merge at all means: nothing
/// has been polled. Removing the last account rebuilds limits.json empty, so
/// the file existing is not proof there is anything to show.
fn load(dirs: &LimitsDirs) -> Result<Merged> {
    snapshot::load_merged(dirs)?
        .filter(|merged| !merged.accounts.is_empty())
        .ok_or_else(|| LimitsError::Message("no snapshots yet — run: ovm limits poll".into()))
}

/// An account that is not signed in cannot have produced these numbers from
/// the home it polls now — an upgrade moved every account into a home of its
/// own, and its old snapshot outlived it. Say so rather than let stale
/// figures read as current.
fn stale_accounts(dirs: &LimitsDirs) -> Result<Vec<String>> {
    Ok(crate::registry::Registry::load(&dirs.config_file())?
        .map(|config| {
            config
                .accounts
                .iter()
                .filter(|account| !account.signed_in(dirs))
                .map(|account| account.display())
                .collect()
        })
        .unwrap_or_default())
}

/// `show <id or label>`: one account, every window, every detail.
pub fn run_account(dirs: &LimitsDirs, key: &str) -> Result<()> {
    let merged = load(dirs)?;
    let account = merged
        .accounts
        .iter()
        .find(|account| account.id == key || account.label.as_deref() == Some(key))
        .ok_or_else(|| LimitsError::Message(format!("no snapshot for `{key}`")))?;
    let stale = stale_accounts(dirs)?;
    let registry = Registry::load_or_default(&dirs.config_file())?;
    let plan = recorded_plan(&registry, account);
    print!("{}", render_account(account, snapshot::now(), &stale, plan));
    Ok(())
}

pub fn run(dirs: &LimitsDirs, format: Format, overrides: Overrides) -> Result<()> {
    let merged = load(dirs)?;
    match format {
        Format::Json => {
            let registry = Registry::load_or_default(&dirs.config_file())?;
            let json = with_recorded_plans(&merged, &registry)?;
            println!("{}", serde_json::to_string_pretty(&json)?);
            return Ok(());
        }
        Format::Brief => {
            println!(
                "{}",
                render_plain(&merged, snapshot::now(), &snapshot::hostname())
            );
            return Ok(());
        }
        Format::Grid => {
            print!("{}", render_grid(&merged, snapshot::now()));
            return Ok(());
        }
        Format::Table => {}
    }
    let stale = stale_accounts(dirs)?;
    let registry = Registry::load_or_default(&dirs.config_file())?;
    let arrangement = overrides.apply(registry.arrangement());
    // A terminal gets lines that fit it; a pipe or a file gets them whole.
    let stdout = ovm_tui::Term::stdout();
    let width = if stdout.is_term() {
        ovm_tui::terminal_width(&stdout)
    } else {
        usize::MAX
    };
    print!(
        "{}",
        render(
            &merged,
            snapshot::now(),
            &stale,
            width,
            arrangement,
            &registry
        )
    );
    Ok(())
}

/// The key a recorded plan goes under in `show --json`. Not `plan`: that
/// key is already the product's own plan type (Codex's `pro`), and the JSON
/// only ever grows.
pub const RECORDED_PLAN_KEY: &str = "subscription";

/// The merged file as `show --json` prints it: unchanged, plus a
/// [`RECORDED_PLAN_KEY`] object on each account that has a plan recorded.
pub fn with_recorded_plans(merged: &Merged, registry: &Registry) -> Result<serde_json::Value> {
    let mut json = serde_json::to_value(merged)?;
    let Some(accounts) = json
        .get_mut("accounts")
        .and_then(serde_json::Value::as_array_mut)
    else {
        return Ok(json);
    };
    for (account, snapshot) in accounts.iter_mut().zip(&merged.accounts) {
        let (Some(object), Some(plan)) =
            (account.as_object_mut(), recorded_plan(registry, snapshot))
        else {
            continue;
        };
        object.insert(RECORDED_PLAN_KEY.to_string(), serde_json::to_value(plan)?);
    }
    Ok(json)
}

/// The account table, the same one the registry screen draws. Pure so the
/// layout is testable against a fixed clock. `stale` names the accounts whose
/// home has no login, whose numbers therefore predate it; `registry` holds
/// the plans the notes mention.
pub fn render(
    merged: &Merged,
    now: u64,
    stale: &[String],
    width: usize,
    arrangement: table::Arrangement,
    registry: &Registry,
) -> String {
    let mut out = String::new();
    if merged.accounts.is_empty() {
        out.push_str("  no accounts polled yet\n");
        return out;
    }
    let mut accounts: Vec<&AccountSnapshot> = merged.accounts.iter().collect();
    accounts
        .sort_by(|a, b| table::order(arrangement, (a.provider, Some(a)), (b.provider, Some(b))));
    let lines: Vec<Line> = accounts
        .iter()
        .map(|account| table_line(account, recorded_plan(registry, account)))
        .collect();
    let options = table::Options {
        now,
        width,
        secondary: false,
        cursor: None,
        grouped: arrangement.grouped,
    };
    for line in table::render(&lines, &options) {
        let _ = writeln!(out, "{line}");
    }
    for account in accounts
        .iter()
        .filter(|account| stale.contains(&account.display()))
    {
        let _ = writeln!(
            out,
            "\n  {} {} not signed in — these numbers predate its own home; run: ovm limits login {}",
            style("!").yellow(),
            table_name(account),
            account.id
        );
    }
    out
}

/// The label, or the id when there is none.
pub fn table_name(account: &AccountSnapshot) -> &str {
    account.label.as_deref().unwrap_or(&account.id)
}

/// The plan recorded for a snapshot's account, if the registry has one.
pub fn recorded_plan<'a>(registry: &'a Registry, account: &AccountSnapshot) -> Option<&'a Plan> {
    registry
        .find(&account.id)
        .and_then(|registered| registered.plan.as_ref())
}

/// A snapshot as a table row: its numbers, or why there are none.
pub fn table_line<'a>(account: &'a AccountSnapshot, plan: Option<&'a Plan>) -> Line<'a> {
    let cells = if let Some(error) = &account.error {
        Cells::Error(error)
    } else if account.windows.is_empty() {
        Cells::Message("no usage windows reported")
    } else {
        Cells::Usage(account)
    };
    Line {
        name: table_name(account),
        provider: account.provider,
        cells,
        plan,
    }
}

/// One account in full: every window, credits, the poll it came from, and
/// the plan the person recorded for it.
pub fn render_account(
    account: &AccountSnapshot,
    now: u64,
    stale: &[String],
    plan: Option<&Plan>,
) -> String {
    let mut out = String::new();
    let mut title = account.display();
    if let Some(plan) = &account.plan {
        title.push_str(&format!(" ({plan})"));
    }
    if let Some(tag) = account.kind_tag() {
        title.push_str(&format!("  [{tag}]"));
    }
    out.push_str(&format!(
        "  {}  {}\n",
        style(title).bold(),
        style(format!(
            "captured {} on {}",
            ago(now, account.captured_at),
            account.host
        ))
        .dim()
    ));
    if stale.iter().any(|label| *label == account.display()) {
        out.push_str(&format!(
            "    {} not signed in — these numbers predate this account's own home; run: ovm limits login {}\n",
            style("!").yellow(),
            account.display()
        ));
    }
    if let Some(plan) = plan {
        let line = format!("plan: {}", plan_line(plan, table::local_day(now)));
        let _ = writeln!(out, "    {}", style(line).dim());
    }
    if let Some(error) = &account.error {
        out.push_str(&format!("    {} {error}\n", style("✗").red()));
        return out;
    }
    if account.windows.is_empty() {
        out.push_str("    (no usage windows reported)\n");
    }
    let label_width = account
        .windows
        .iter()
        .map(|w| w.label.chars().count())
        .max()
        .unwrap_or(0)
        .max(2);
    for window in ordered_windows(&account.windows) {
        out.push_str(&format!(
            "    {}\n",
            render_window(window, now, label_width)
        ));
    }
    for line in detail_lines(account, now) {
        let _ = writeln!(out, "    {}", style(line).dim());
    }
    out
}

/// What else the product said, one line each: plan and credits for Codex,
/// the model and version the poll ran on for Claude.
fn detail_lines(account: &AccountSnapshot, now: u64) -> Vec<String> {
    let mut lines = Vec::new();
    for credit in &account.reset_credits {
        let mut line = format!(
            "reset credit: {}",
            credit.title.as_deref().unwrap_or("reset")
        );
        if let Some(status) = &credit.status {
            let _ = write!(line, " ({status}");
            match credit.expires_at {
                Some(at) if at > now => {
                    let _ = write!(line, ", expires in {}", human(at - now));
                }
                Some(_) => line.push_str(", expired"),
                None => {}
            }
            line.push(')');
        }
        lines.push(line);
    }
    if account.reset_credits.is_empty() {
        if let Some(count) = account.reset_credits_available.filter(|c| *c > 0) {
            lines.push(format!("{count} reset credit(s) available"));
        }
    }
    if let Some(credits) = &account.credits {
        if credits.unlimited {
            lines.push("credits: unlimited".into());
        } else if credits.has_credits {
            lines.push(format!(
                "credits: {}",
                credits.balance.as_deref().unwrap_or("some")
            ));
        }
    }
    if account.spend_control_reached == Some(true) {
        lines.push("spend control reached".into());
    }
    if let Some(kind) = &account.rate_limit_reached_type {
        lines.push(format!("rate limit reached: {kind}"));
    }
    let mut poll = Vec::new();
    if let Some(model) = &account.model {
        poll.push(format!("on {model}"));
    }
    if let Some(version) = &account.product_version {
        poll.push(format!("Claude Code {version}"));
    }
    // A subscription poll is a turn, not a bill: the statusline's estimate is
    // often 0 and worth a line only when it is not.
    if let Some(cost) = account.poll_cost_usd.filter(|cost| *cost > 0.0) {
        poll.push(format!("${cost:.4}"));
    }
    if !poll.is_empty() {
        lines.push(format!("poll: {}", poll.join(", ")));
    }
    lines
}

/// The long window first. A weekly allowance is the one that runs out for
/// good; a 5h window is back in five hours. Longest-first puts 7d (Claude)
/// and 1w (Codex) at the top without either provider's labels being special
/// cased — and windows whose length the product never told us sort last, in
/// the order it listed them.
pub fn ordered_windows(windows: &[Window]) -> Vec<&Window> {
    let mut ordered: Vec<&Window> = windows.iter().collect();
    ordered.sort_by_key(|w| std::cmp::Reverse(w.window_minutes.unwrap_or(0)));
    ordered
}

/// Red past 90% used, yellow past 70% — the grid's bars.
fn severity<T: std::fmt::Display>(text: T, used_percent: f64) -> String {
    if used_percent >= 90.0 {
        style(text).red().to_string()
    } else if used_percent >= 70.0 {
        style(text).yellow().to_string()
    } else {
        text.to_string()
    }
}

/// `7d   93% left   resets back Thu 14:00 · in 6d` — the table's words, one
/// window per line, for the detail view.
fn render_window(window: &Window, now: u64, label_width: usize) -> String {
    let left = table::left_percent(window);
    let left_text = table::left_colour(left, format!("{:>4}", format!("{left}%")));
    let reset = match window.resets_at {
        Some(at) if at > now => format!(
            "resets back {} · {}",
            table::clock(at, now),
            table::countdown(at - now)
        ),
        Some(at) => format!(
            "rolled over {} · {} ago",
            table::clock(at, now),
            human(now - at)
        ),
        None => "reset time not reported".to_string(),
    };
    format!(
        "{:<label_width$}  {left_text} left   {}",
        window.label,
        style(reset).dim()
    )
}

/// `7d 96% used, 4% left, resets Thu 24 Sep 22:00 (4d 5h)` — every fact on
/// one breath, for the digest a phone shows.
pub fn window_brief(window: &Window, now: u64) -> String {
    let used = window.used_percent.clamp(0.0, 100.0).round() as i64;
    let tail = match window.resets_at {
        Some(at) if at > now => format!(", resets {} ({})", local_stamp(at), human(at - now)),
        Some(at) => format!(", reset {} ago", human(now - at)),
        None => String::new(),
    };
    format!("{} {used}% used, {}% left{tail}", window.label, 100 - used)
}

/// The digest: every account, every window, no colour and no terminal
/// assumptions — this is what gets piped into a push notification. Same
/// words and same order as the table, so the phone and the terminal agree.
pub fn render_plain(merged: &Merged, now: u64, host: &str) -> String {
    let mut out = String::new();
    for account in &merged.accounts {
        // The push title already names the sending machine; an account only
        // gets its own tag when another machine captured it.
        let mut title = if account.host == host {
            account.display()
        } else {
            format!("[{}] {}", account.host, account.display())
        };
        if let Some(plan) = &account.plan {
            let _ = write!(title, " · {plan}");
        }
        if let Some(tag) = account.kind_tag() {
            let _ = write!(title, " · {tag}");
        }
        let _ = writeln!(out, "{title}");
        if let Some(error) = &account.error {
            let _ = writeln!(out, "  not polling: {}", brief_error(error));
            continue;
        }
        if account.windows.is_empty() {
            out.push_str("  no usage windows reported\n");
        }
        for window in ordered_windows(&account.windows) {
            let _ = writeln!(out, "  {}", window_brief(window, now));
        }
    }
    out.trim_end().to_string()
}

/// Width of one window column in the grid, bar included.
const GRID_CELL: usize = 30;
/// Width of the account column in the grid.
const GRID_NAME: usize = 24;

/// Every account on one screen, the way a person compares them: grouped by
/// provider with a count, one row per account (label over plan), one column
/// per window — name and percent, a bar, then when it turns over — and for
/// Codex a column for the manual reset credits the account holds. The weekly
/// window leads, as everywhere else.
pub fn render_grid(merged: &Merged, now: u64) -> String {
    let mut out = String::new();
    if merged.accounts.is_empty() {
        out.push_str("  no accounts polled yet\n");
        return out;
    }
    for provider in [
        crate::registry::Provider::Claude,
        crate::registry::Provider::Codex,
    ] {
        let accounts: Vec<&AccountSnapshot> = merged
            .accounts
            .iter()
            .filter(|account| account.provider == provider)
            .collect();
        if accounts.is_empty() {
            continue;
        }
        let _ = writeln!(
            out,
            "\n  {}  {}\n",
            style(provider.display_name()).bold(),
            style(accounts.len()).dim()
        );
        for account in accounts {
            out.push_str(&grid_row(account, now));
            out.push('\n');
        }
    }
    out
}

/// One account: three terminal lines, the name column and every cell side by side.
fn grid_row(account: &AccountSnapshot, now: u64) -> String {
    let name = [
        style(pad(&account.display(), GRID_NAME)).bold().to_string(),
        style(pad(
            &account
                .kind_tag()
                .or_else(|| account.plan.clone())
                .unwrap_or_default(),
            GRID_NAME,
        ))
        .dim()
        .to_string(),
        " ".repeat(GRID_NAME),
    ];
    let mut cells: Vec<[String; 3]> = Vec::new();
    if let Some(error) = &account.error {
        cells.push([
            style(pad("✗ not polling", GRID_CELL)).red().to_string(),
            pad(&brief_error(error), GRID_CELL * 2),
            String::new(),
        ]);
    } else if account.windows.is_empty() {
        cells.push([
            pad("no usage windows reported", GRID_CELL),
            String::new(),
            String::new(),
        ]);
    } else {
        for window in ordered_windows(&account.windows) {
            cells.push(grid_window(window, now));
        }
        if let Some(credits) = grid_reset_credits(account, now) {
            cells.push(credits);
        }
    }
    let mut out = String::new();
    for line in 0..3 {
        let mut text = format!("  {}", name[line]);
        for cell in &cells {
            text.push_str("  ");
            text.push_str(&cell[line]);
        }
        let _ = writeln!(out, "{}", text.trim_end());
    }
    out
}

/// `7-day limit          96%` / bar / `in 3d 20h · Thu 24 Sep 22:00`.
fn grid_window(window: &Window, now: u64) -> [String; 3] {
    let used = window.used_percent.clamp(0.0, 100.0);
    let percent = format!("{}%", used.round() as i64);
    let title = window_title(window);
    let gap = GRID_CELL.saturating_sub(title.chars().count() + percent.chars().count());
    let head = format!("{title}{}{}", " ".repeat(gap), style(percent).bold());
    let filled = ((used / 100.0) * GRID_CELL as f64).round() as usize;
    let filled = filled.min(GRID_CELL);
    let bar = severity(
        format!(
            "{}{}",
            "━".repeat(filled),
            style("━".repeat(GRID_CELL - filled)).dim()
        ),
        used,
    );
    let when = match window.resets_at {
        Some(at) if at > now => format!("in {} · {}", human(at - now), local_stamp(at)),
        Some(_) => "reset since last poll".to_string(),
        None => "No reset pending".to_string(),
    };
    [head, bar, style(pad(&when, GRID_CELL)).dim().to_string()]
}

/// `7-day limit`, `5-hour limit`, `Weekly limit` — named from the window's
/// length where the product said it, from its own label where it did not.
fn window_title(window: &Window) -> String {
    const HOUR: u64 = 60;
    const DAY: u64 = 24 * HOUR;
    let name = match window.window_minutes {
        Some(minutes) if minutes == 7 * DAY && window.id.starts_with("codex") => {
            "Weekly".to_string()
        }
        Some(minutes) if minutes % DAY == 0 => format!("{}-day", minutes / DAY),
        Some(minutes) if minutes % HOUR == 0 => format!("{}-hour", minutes / HOUR),
        _ => window.label.clone(),
    };
    format!("{name} limit")
}

/// `Manual resets` / `1 available` / `next expires in 12d · Sun 04 Oct 21:19`.
fn grid_reset_credits(account: &AccountSnapshot, now: u64) -> Option<[String; 3]> {
    let listed = account
        .reset_credits
        .iter()
        .filter(|credit| credit.status.as_deref().is_none_or(|s| s == "available"))
        .count() as u64;
    let available = account
        .reset_credits_available
        .unwrap_or(listed)
        .max(listed);
    if available == 0 && account.reset_credits.is_empty() {
        return None;
    }
    let next = account
        .reset_credits
        .iter()
        .filter_map(|credit| credit.expires_at)
        .filter(|at| *at > now)
        .min();
    let expiry = next.map_or(String::new(), |at| {
        format!("next expires in {} · {}", human(at - now), local_stamp(at))
    });
    Some([
        pad("Manual resets", GRID_CELL),
        format!(
            "{} {}",
            style(available).bold(),
            pad("available", GRID_CELL - 2)
        ),
        style(pad(&expiry, GRID_CELL + 12)).dim().to_string(),
    ])
}

/// Left-align in `width` columns, cutting with an ellipsis rather than
/// letting one long label push every column after it out of line.
fn pad(text: &str, width: usize) -> String {
    let count = text.chars().count();
    if count > width {
        let cut: String = text.chars().take(width.saturating_sub(1)).collect();
        return format!("{cut}…");
    }
    format!("{text}{}", " ".repeat(width - count))
}

/// A phone shows two or three lines: keep the cause, drop the quoted screen.
/// The full text stays in limits.json and the agent log.
const BRIEF_ERROR_CHARS: usize = 160;

fn brief_error(error: &str) -> String {
    let cause = error
        .split(" (last on screen:")
        .next()
        .unwrap_or(error)
        .trim();
    if cause.chars().count() <= BRIEF_ERROR_CHARS {
        return cause.to_string();
    }
    let cut: String = cause.chars().take(BRIEF_ERROR_CHARS).collect();
    format!("{}…", cut.trim_end())
}

/// `Thu 10 Sep 14:00`, in the machine's own time zone. Epoch seconds are
/// what the file carries; this is what a person reads beside them.
pub fn local_stamp(epoch: u64) -> String {
    let time = epoch as libc::time_t;
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: both pointers are valid for the call; localtime_r writes tm.
    if unsafe { libc::localtime_r(&time, &mut tm) }.is_null() {
        return String::new();
    }
    let mut buffer = [0u8; 64];
    let format = c"%a %d %b %H:%M";
    // SAFETY: the buffer is valid for its declared length and the format is
    // NUL-terminated; strftime writes at most that many bytes.
    let written = unsafe {
        libc::strftime(
            buffer.as_mut_ptr() as *mut libc::c_char,
            buffer.len(),
            format.as_ptr(),
            &tm,
        )
    };
    String::from_utf8_lossy(&buffer[..written]).into_owned()
}

/// `Example Pro 100 · cancelled · access until Fri 30 Oct 2026 (in 28d)`:
/// a recorded plan in words. `today` is a day number on the local calendar
/// ([`table::local_day`]).
pub fn plan_line(plan: &Plan, today: i64) -> String {
    let mut parts = Vec::new();
    if let Some(name) = &plan.name {
        parts.push(name.clone());
    }
    parts.push(plan.status.to_string());
    if let Some(date) = plan.ends_on {
        let days_left = date.days() - today;
        let ended = days_left < 0;
        let verb = match (plan.status, ended) {
            (PlanStatus::Cancelled, false) => "access until",
            (PlanStatus::Cancelled, true) => "access ended",
            (PlanStatus::Live, false) => "renews",
            (PlanStatus::Live, true) => "renewed",
        };
        parts.push(format!(
            "{verb} {} ({})",
            table::full_date(date),
            days_away(days_left)
        ));
    }
    parts.join(" · ")
}

/// `today`, `in 28d`, `2d ago`.
fn days_away(days: i64) -> String {
    match days {
        0 => "today".to_string(),
        _ if days > 0 => format!("in {days}d"),
        _ => format!("{}d ago", -days),
    }
}

fn ago(now: u64, then: u64) -> String {
    if then > now {
        return "just now".into();
    }
    let delta = now - then;
    if delta < 60 {
        "just now".into()
    } else {
        format!("{} ago", human(delta))
    }
}

/// `2h 13m`, `6d 12h`, `45m`, `30s`.
pub fn human(seconds: u64) -> String {
    let days = seconds / 86_400;
    let hours = (seconds % 86_400) / 3_600;
    let minutes = (seconds % 3_600) / 60;
    if days > 0 {
        format!("{days}d {hours}h")
    } else if hours > 0 {
        format!("{hours}h {minutes}m")
    } else if minutes > 0 {
        format!("{minutes}m")
    } else {
        format!("{seconds}s")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Provider;

    fn merged() -> Merged {
        Merged {
            schema: snapshot::SCHEMA.into(),
            generated_at: 1_000_000,
            host: "m5".into(),
            last_poll_at: Some(1_000_000),
            next_poll_at: None,
            interval_minutes: 60,
            events: Vec::new(),
            accounts: vec![
                AccountSnapshot {
                    captured_at: 1_000_000 - 180,
                    host: "m5".into(),
                    model: Some("claude-haiku-4-5".into()),
                    product_version: Some("2.1.263".into()),
                    windows: vec![
                        Window {
                            id: "five_hour".into(),
                            label: "5h".into(),
                            used_percent: 19.0,
                            resets_at: Some(1_000_000 + 7_980),
                            window_minutes: Some(300),
                            last_reset_at: None,
                        },
                        Window {
                            id: "seven_day".into(),
                            label: "7d".into(),
                            used_percent: 92.0,
                            resets_at: Some(1_000_000 - 60),
                            window_minutes: Some(10_080),
                            last_reset_at: None,
                        },
                    ],
                    poll_cost_usd: Some(0.06),
                    ..AccountSnapshot::empty(
                        Provider::Claude,
                        "claude-1",
                        Some("work".into()),
                        "statusline",
                    )
                },
                AccountSnapshot {
                    captured_at: 1_000_000,
                    host: "mini".into(),
                    plan: Some("pro".into()),
                    reset_credits_available: Some(1),
                    error: Some("signed out".into()),
                    ..AccountSnapshot::empty(Provider::Codex, "codex-1", None, "app-server")
                },
            ],
        }
    }

    fn strings(args: &[&str]) -> Vec<String> {
        args.iter().map(|arg| arg.to_string()).collect()
    }

    #[test]
    fn arrangement_flags_come_out_and_the_rest_stays_in_order() {
        let (overrides, rest) =
            Overrides::take(&strings(&["--sort", "reset", "--json", "--no-group"])).unwrap();
        assert_eq!(overrides.sort, Some(table::SortOrder::Reset));
        assert_eq!(overrides.grouped, Some(false));
        assert_eq!(rest, ["--json"]);

        let (overrides, rest) = Overrides::take(&strings(&["--group"])).unwrap();
        assert_eq!(
            overrides,
            Overrides {
                sort: None,
                grouped: Some(true)
            }
        );
        assert!(rest.is_empty());

        assert!(Overrides::take(&strings(&["--sort"])).is_err());
        assert!(Overrides::take(&strings(&["--sort", "name"])).is_err());
    }

    #[test]
    fn a_flag_wins_over_the_setting_and_no_flag_keeps_it() {
        let persisted = table::Arrangement {
            sort: table::SortOrder::Reset,
            grouped: true,
        };
        assert_eq!(Overrides::default().apply(persisted), persisted);
        let flags = Overrides {
            sort: Some(table::SortOrder::Provider),
            grouped: Some(false),
        };
        assert_eq!(flags.apply(persisted), table::Arrangement::default());
        let sort_only = Overrides {
            sort: Some(table::SortOrder::Provider),
            grouped: None,
        };
        assert_eq!(
            sort_only.apply(persisted),
            table::Arrangement {
                sort: table::SortOrder::Provider,
                grouped: true,
            }
        );
    }

    #[test]
    fn the_table_is_one_row_per_account_in_provider_order() {
        console::set_colors_enabled(false);
        let mut merged = merged();
        // Codex listed first in the file; the table still puts Claude first.
        merged.accounts.reverse();
        let text = render(
            &merged,
            1_000_000,
            &[],
            100,
            table::Arrangement::default(),
            &Registry::default(),
        );
        let lines: Vec<&str> = text.lines().map(str::trim_end).collect();
        assert_eq!(lines.len(), 3, "{text}");
        assert_eq!(lines[0], "                      left   resets back");
        // The weekly window is the primary; it rolled over since the poll.
        assert!(
            lines[1].starts_with("  work       claude     8%   "),
            "{text}"
        );
        assert!(lines[1].ends_with("rolled over"), "{text}");
        assert!(!text.contains("19%"), "the 5h window stays hidden: {text}");
        assert_eq!(lines[2], "  codex-1    codex    ✗ signed out");
    }

    #[test]
    fn the_table_note_carries_the_plan_the_registry_records() {
        console::set_colors_enabled(false);
        let mut registry = Registry::default();
        registry
            .push(crate::registry::Account::new(
                crate::registry::Provider::Claude,
                "claude-1",
                Some("work".into()),
            ))
            .unwrap();
        let cancelled = plan(None, PlanStatus::Cancelled, Some("2026-10-30"));
        registry.set_plan("work", Some(cancelled)).unwrap();
        let text = render(
            &merged(),
            1_000_000,
            &[],
            200,
            table::Arrangement::default(),
            &registry,
        );
        let work = text.lines().find(|line| line.contains("work")).unwrap();
        assert!(work.trim_end().ends_with("ends 30 Oct"), "{text}");
        let codex = text.lines().find(|line| line.contains("codex-1")).unwrap();
        assert!(!codex.contains("ends"), "{text}");
    }

    #[test]
    fn a_stale_account_is_named_under_the_table() {
        console::set_colors_enabled(false);
        let text = render(
            &merged(),
            1_000_000,
            &["claude-1 (work)".into()],
            100,
            table::Arrangement::default(),
            &Registry::default(),
        );
        assert!(
            text.contains("! work not signed in — these numbers predate its own home; run: ovm limits login claude-1"),
            "{text}"
        );
    }

    #[test]
    fn the_detail_lists_every_window_in_the_table_words() {
        console::set_colors_enabled(false);
        let merged = merged();
        let text = render_account(&merged.accounts[0], 1_000_000, &[], None);
        assert!(text.contains("claude-1 (work)"), "{text}");
        assert!(text.contains("captured 3m ago on m5"), "{text}");
        // The weekly window is the one that runs out for good, so it leads.
        let weekly = text.find("7d").expect("weekly window");
        let five_hour = text.find("5h").expect("5h window");
        assert!(weekly < five_hour, "{text}");
        assert!(text.contains("7d    8% left   rolled over "), "{text}");
        assert!(text.contains("· 1m ago"), "{text}");
        assert!(text.contains("5h   81% left   resets back "), "{text}");
        assert!(text.contains("· in 2h 13m"), "{text}");
        assert!(!text.contains("used"), "{text}");
        assert!(
            text.contains("poll: on claude-haiku-4-5, Claude Code 2.1.263, $0.0600"),
            "{text}"
        );
        let codex = render_account(&merged.accounts[1], 1_000_000, &[], None);
        assert!(codex.contains("codex-1 (pro)"), "{codex}");
        assert!(codex.contains("✗ signed out"), "{codex}");
    }

    #[test]
    fn reset_credits_and_credit_state_get_a_line_each() {
        console::set_colors_enabled(false);
        let mut merged = merged();
        let codex = &mut merged.accounts[1];
        codex.error = None;
        codex.reset_credits = vec![crate::snapshot::ResetCredit {
            id: "c1".into(),
            reset_type: None,
            status: Some("available".into()),
            granted_at: None,
            expires_at: Some(1_000_000 + 30 * 86_400),
            title: Some("Full reset".into()),
            description: None,
        }];
        codex.credits = Some(crate::snapshot::Credits {
            has_credits: true,
            unlimited: false,
            balance: Some("12.50".into()),
        });
        codex.spend_control_reached = Some(true);
        let text = render_account(codex, 1_000_000, &[], None);
        assert!(
            text.contains("reset credit: Full reset (available, expires in 30d 0h)"),
            "{text}"
        );
        assert!(text.contains("credits: 12.50"), "{text}");
        assert!(text.contains("spend control reached"), "{text}");
    }

    #[test]
    fn the_grid_groups_by_provider_and_gives_every_window_a_column() {
        console::set_colors_enabled(false);
        let mut merged = merged();
        merged.accounts[1].error = None;
        merged.accounts[1].windows = vec![Window {
            id: "codex.primary".into(),
            label: "codex 1w".into(),
            used_percent: 85.0,
            resets_at: Some(1_000_000 + 3 * 86_400),
            window_minutes: Some(10_080),
            last_reset_at: None,
        }];
        let text = render_grid(&merged, 1_000_000);
        let claude = text.find("Claude Code  1").expect("claude group");
        let codex = text.find("Codex  1").expect("codex group");
        assert!(claude < codex, "{text}");
        // The long window leads, and every window says when it turns over.
        let row = &text[claude..codex];
        assert!(row.find("7-day limit") < row.find("5-hour limit"), "{text}");
        assert!(row.contains("92%") && row.contains("19%"), "{text}");
        assert!(row.contains("in 2h 13m"), "{text}");
        assert!(
            text.contains("Weekly limit") && text.contains("85%"),
            "{text}"
        );
        assert!(
            text.contains("Manual resets") && text.contains("1 available"),
            "{text}"
        );
        assert!(text.contains("pro"), "{text}");
    }

    #[test]
    fn the_digest_tags_only_foreign_hosts_and_keeps_errors_short() {
        let mut merged = merged();
        merged.accounts[1].error = Some(format!(
            "claude never ran the statusline — {} (last on screen: logo)",
            "x".repeat(300)
        ));
        let text = render_plain(&merged, 1_000_000, "m5");
        assert!(text.starts_with("claude-1 (work)"), "{text}");
        assert!(text.contains("[mini] codex-1 · pro"), "{text}");
        assert!(!text.contains("last on screen"), "{text}");
        assert!(text.contains('…'), "{text}");
    }

    #[test]
    fn a_window_brief_says_used_left_and_when_it_resets() {
        let window = Window {
            id: "five_hour".into(),
            label: "5h".into(),
            used_percent: 26.4,
            resets_at: Some(1_000_000 + 4 * 3600 + 10 * 60),
            window_minutes: Some(300),
            last_reset_at: None,
        };
        assert_eq!(
            window_brief(&window, 1_000_000),
            format!(
                "5h 26% used, 74% left, resets {} (4h 10m)",
                local_stamp(1_000_000 + 4 * 3600 + 10 * 60)
            )
        );
        assert_eq!(
            window_brief(&window, 1_000_000 + 5 * 3600),
            "5h 26% used, 74% left, reset 50m ago"
        );
        let no_reset = Window {
            resets_at: None,
            ..window
        };
        assert_eq!(window_brief(&no_reset, 1_000_000), "5h 26% used, 74% left");
    }

    fn plan(name: Option<&str>, status: PlanStatus, ends_on: Option<&str>) -> Plan {
        Plan {
            name: name.map(str::to_string),
            status,
            ends_on: ends_on.and_then(crate::plan::PlanDate::parse),
        }
    }

    /// Fri 02 Oct 2026 as a local day number.
    fn today() -> i64 {
        crate::plan::PlanDate::parse("2026-10-02").unwrap().days()
    }

    #[test]
    fn the_plan_line_names_the_plan_its_state_and_its_last_day() {
        let cancelled = plan(
            Some("Example Pro 100"),
            PlanStatus::Cancelled,
            Some("2026-10-30"),
        );
        assert_eq!(
            plan_line(&cancelled, today()),
            "Example Pro 100 · cancelled · access until Fri 30 Oct 2026 (in 28d)"
        );
        let ended = plan(None, PlanStatus::Cancelled, Some("2026-09-30"));
        assert_eq!(
            plan_line(&ended, today()),
            "cancelled · access ended Wed 30 Sep 2026 (2d ago)"
        );
        let last_day = plan(None, PlanStatus::Cancelled, Some("2026-10-02"));
        assert_eq!(
            plan_line(&last_day, today()),
            "cancelled · access until Fri 02 Oct 2026 (today)"
        );
        let undated = plan(Some("Example Max"), PlanStatus::Cancelled, None);
        assert_eq!(plan_line(&undated, today()), "Example Max · cancelled");
        let renewing = plan(Some("Example Max"), PlanStatus::Live, Some("2026-11-02"));
        assert_eq!(
            plan_line(&renewing, today()),
            "Example Max · live · renews Mon 02 Nov 2026 (in 31d)"
        );
        assert_eq!(plan_line(&Plan::default(), today()), "live");
    }

    #[test]
    fn the_detail_view_carries_the_plan_line_even_for_a_failed_poll() {
        let merged = merged();
        let cancelled = plan(
            Some("Example Pro 100"),
            PlanStatus::Cancelled,
            Some("2026-10-30"),
        );
        let now = 1_790_947_560;
        let text = console::strip_ansi_codes(&render_account(
            &merged.accounts[0],
            now,
            &[],
            Some(&cancelled),
        ))
        .into_owned();
        assert!(
            text.contains("    plan: Example Pro 100 · cancelled · access until Fri 30 Oct 2026"),
            "{text}"
        );
        let mut failed = merged.accounts[0].clone();
        failed.error = Some("boom".into());
        let text = render_account(&failed, now, &[], Some(&cancelled));
        assert!(text.contains("plan: Example Pro 100"), "{text}");
        let without = render_account(&merged.accounts[0], now, &[], None);
        assert!(!without.contains("plan:"), "{without}");
    }

    #[test]
    fn the_local_stamp_is_a_readable_day_and_time() {
        let stamp = local_stamp(1_000_000);
        // Zone-dependent, so only the shape is pinned: `Mon 12 Jan 13:46`.
        let parts: Vec<&str> = stamp.split(' ').collect();
        assert_eq!(parts.len(), 4, "{stamp}");
        assert!(parts[3].contains(':'), "{stamp}");
    }

    #[test]
    fn human_durations() {
        assert_eq!(human(30), "30s");
        assert_eq!(human(45 * 60), "45m");
        assert_eq!(human(2 * 3600 + 13 * 60), "2h 13m");
        assert_eq!(human(6 * 86400 + 12 * 3600 + 5), "6d 12h");
    }

    #[test]
    fn an_empty_merge_says_so() {
        let mut empty = merged();
        empty.accounts.clear();
        assert_eq!(
            render(
                &empty,
                0,
                &[],
                80,
                table::Arrangement::default(),
                &Registry::default()
            ),
            "  no accounts polled yet\n"
        );
    }
}
