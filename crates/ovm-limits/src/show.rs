//! `ovm limits show` — the merged view as a table, or the file itself.

use crate::paths::LimitsDirs;
use crate::snapshot::{self, AccountSnapshot, Merged, Window};
use crate::{LimitsError, Result};
use console::style;
use std::fmt::Write as _;

pub fn run(dirs: &LimitsDirs, as_json: bool) -> Result<()> {
    // A merge with no accounts in it means what no merge at all means: nothing
    // has been polled. Removing the last account rebuilds limits.json empty, so
    // the file existing is not proof there is anything to show.
    let merged = snapshot::load_merged(dirs)?
        .filter(|merged| !merged.accounts.is_empty())
        .ok_or_else(|| LimitsError::Message("no snapshots yet — run: ovm limits poll".into()))?;
    if as_json {
        println!("{}", serde_json::to_string_pretty(&merged)?);
        return Ok(());
    }
    // An account that is not signed in cannot have produced these numbers from
    // the home it polls now — an upgrade moved every account into a home of its
    // own, and its old snapshot outlived it. Say so rather than let stale
    // figures read as current.
    let stale: Vec<String> = crate::registry::Registry::load(&dirs.config_file())?
        .map(|config| {
            config
                .accounts
                .iter()
                .filter(|account| !account.signed_in(dirs))
                .map(|account| account.display())
                .collect()
        })
        .unwrap_or_default();
    print!("{}", render(&merged, snapshot::now(), &stale));
    Ok(())
}

/// Pure so the layout is testable against a fixed clock. `stale` names the
/// accounts whose home has no login, whose numbers therefore predate it.
pub fn render(merged: &Merged, now: u64, stale: &[String]) -> String {
    let mut out = String::new();
    if merged.accounts.is_empty() {
        out.push_str("  no accounts polled yet\n");
        return out;
    }
    for account in &merged.accounts {
        out.push_str(&render_account(account, now, stale));
    }
    out
}

fn render_account(account: &AccountSnapshot, now: u64, stale: &[String]) -> String {
    let mut out = String::new();
    let mut title = account.display();
    if let Some(plan) = &account.plan {
        title.push_str(&format!(" ({plan})"));
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
    for window in &account.windows {
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

fn render_window(window: &Window, now: u64, label_width: usize) -> String {
    let used = format!("{:>3}%", window.used_percent.round() as i64);
    let used = if window.used_percent >= 90.0 {
        style(used).red().to_string()
    } else if window.used_percent >= 70.0 {
        style(used).yellow().to_string()
    } else {
        used
    };
    let reset = match window.resets_at {
        Some(at) if at > now => format!("resets in {} ({})", human(at - now), local_stamp(at)),
        Some(at) => format!("window reset {} ago ({})", human(now - at), local_stamp(at)),
        None => String::new(),
    };
    format!(
        "{:<label_width$}  {used}  {}",
        window.label,
        style(reset).dim()
    )
}

/// `5h 26% (resets in 4h 10m)` — one window on one breath, for the poll
/// narration and the registry table.
pub fn window_brief(window: &Window, now: u64) -> String {
    let used = window.used_percent.round() as i64;
    match window.resets_at {
        Some(at) if at > now => format!("{} {used}% (resets in {})", window.label, human(at - now)),
        Some(_) => format!("{} {used}% (reset)", window.label),
        None => format!("{} {used}%", window.label),
    }
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

    #[test]
    fn the_table_shows_percent_countdown_age_and_errors() {
        console::set_colors_enabled(false);
        let text = render(&merged(), 1_000_000, &[]);
        assert!(text.contains("claude-1 (work)"), "{text}");
        assert!(text.contains("captured 3m ago on m5"), "{text}");
        assert!(text.contains("5h   19%  resets in 2h 13m"), "{text}");
        assert!(text.contains("7d   92%  window reset 1m ago"), "{text}");
        assert!(
            text.contains("poll: on claude-haiku-4-5, Claude Code 2.1.263, $0.0600"),
            "{text}"
        );
        assert!(text.contains("codex-1 (pro)"), "{text}");
        assert!(text.contains("✗ signed out"), "{text}");
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
        let text = render(&merged, 1_000_000, &[]);
        assert!(
            text.contains("reset credit: Full reset (available, expires in 30d 0h)"),
            "{text}"
        );
        assert!(text.contains("credits: 12.50"), "{text}");
        assert!(text.contains("spend control reached"), "{text}");
    }

    #[test]
    fn a_window_brief_says_when_it_resets() {
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
            "5h 26% (resets in 4h 10m)"
        );
        assert_eq!(
            window_brief(&window, 1_000_000 + 5 * 3600),
            "5h 26% (reset)"
        );
        let no_reset = Window {
            resets_at: None,
            ..window
        };
        assert_eq!(window_brief(&no_reset, 1_000_000), "5h 26%");
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
        assert_eq!(render(&empty, 0, &[]), "  no accounts polled yet\n");
    }
}
