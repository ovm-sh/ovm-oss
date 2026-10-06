//! The account registry on screen: every registered login, its latest
//! numbers, and the keys that change it. Built on the same primitives as
//! `ovm switch`, so it moves the way the rest of OVM moves.
//!
//! The screen is a list; the actions leave it. Adding, signing in, and
//! polling all print as they go (a browser opens, a session is driven), so
//! the registry steps aside, lets the action narrate on the plain terminal,
//! waits for a key, and comes back with the rows reloaded. Nothing is cached
//! across an action: what is on screen is what is on disk.

use crate::accounts::{self, AddOptions, HomeFate, Selection};
use crate::paths::{display, LimitsDirs};
use crate::registry::{Account, Provider, Registry};
use crate::snapshot::{self, AccountSnapshot};
use crate::table::{self, Arrangement, Cells, Line};
use crate::{agent, show, Result};
use ovm_tui::{
    confirm_inline, footer, press_any_key, read_line_inline, select_one, style, terminal_width,
    Footer, Key, Keys, Screen, Term,
};

/// One account as the screen knows it.
pub struct Row {
    pub account: Account,
    pub signed_in: bool,
    pub snapshot: Option<AccountSnapshot>,
}

/// What a keypress asked for. Pure, so the key handling is testable without
/// a terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Action {
    /// Redraw and keep asking.
    Continue,
    Add,
    Login,
    Rename,
    Remove,
    Details,
    PollAll,
    ToggleAgent,
    Events,
    CycleInterval,
    /// The model has already re-sorted; the new order wants saving.
    CycleSort,
    /// The model has already regrouped; the new grouping wants saving.
    ToggleGroup,
    Quit,
}

struct Model {
    rows: Vec<Row>,
    cursor: usize,
    /// One-shot line under the table, so a key that did something says so.
    notice: Option<String>,
    /// Secondary windows (Claude's 5h, a Codex reserve) on lines of their own.
    secondary: bool,
    /// The row order and grouping, as persisted in the registry.
    arrangement: Arrangement,
}

impl Model {
    /// The rows in table order, as `arrangement` has it.
    fn new(
        rows: Vec<Row>,
        cursor: usize,
        notice: Option<String>,
        arrangement: Arrangement,
    ) -> Self {
        let mut model = Self {
            rows,
            cursor,
            notice,
            secondary: false,
            arrangement,
        };
        model.sort_rows();
        model.cursor = cursor.min(model.rows.len().saturating_sub(1));
        model
    }

    fn sort_rows(&mut self) {
        let arrangement = self.arrangement;
        self.rows.sort_by(|a, b| {
            table::order(
                arrangement,
                (a.account.provider, usage_snapshot(a)),
                (b.account.provider, usage_snapshot(b)),
            )
        });
    }

    /// Re-sort for a new arrangement, the cursor staying on its account.
    fn rearrange(&mut self, arrangement: Arrangement) {
        let under_cursor = self.current().map(|row| row.account.id.clone());
        self.arrangement = arrangement;
        self.sort_rows();
        if let Some(id) = under_cursor {
            if let Some(index) = self.rows.iter().position(|row| row.account.id == id) {
                self.cursor = index;
            }
        }
    }

    fn current(&self) -> Option<&Row> {
        self.rows.get(self.cursor)
    }

    fn handle(&mut self, key: Key) -> Action {
        let has_rows = !self.rows.is_empty();
        match key {
            Key::ArrowUp | Key::Char('k') => {
                self.cursor = self.cursor.saturating_sub(1);
                Action::Continue
            }
            Key::ArrowDown | Key::Char('j') => {
                if self.cursor + 1 < self.rows.len() {
                    self.cursor += 1;
                }
                Action::Continue
            }
            Key::Char('a') | Key::Char('A') => Action::Add,
            Key::Char('b') | Key::Char('B') => Action::ToggleAgent,
            Key::Char('e') | Key::Char('E') => Action::Events,
            Key::Char('i') | Key::Char('I') => Action::CycleInterval,
            Key::Char('w') | Key::Char('W') => {
                self.secondary = !self.secondary;
                Action::Continue
            }
            Key::Char('s') | Key::Char('S') => {
                self.rearrange(Arrangement {
                    sort: self.arrangement.sort.next(),
                    ..self.arrangement
                });
                Action::CycleSort
            }
            Key::Char('g') | Key::Char('G') => {
                self.rearrange(Arrangement {
                    grouped: !self.arrangement.grouped,
                    ..self.arrangement
                });
                Action::ToggleGroup
            }
            Key::Escape | Key::Char('q') | Key::Char('Q') => Action::Quit,
            Key::Enter if has_rows => Action::Details,
            Key::Char('l') | Key::Char('L') if has_rows => Action::Login,
            Key::Char('r') | Key::Char('R') if has_rows => Action::Rename,
            Key::Char('d') | Key::Char('D') if has_rows => Action::Remove,
            Key::Char('p') | Key::Char('P') if has_rows => Action::PollAll,
            _ => Action::Continue,
        }
    }
}

pub fn run(dirs: &LimitsDirs) -> Result<()> {
    let term = Term::stderr();
    let mut cursor = 0usize;
    let mut notice: Option<String> = None;
    let mut secondary = false;
    loop {
        let (rows, arrangement) = load_rows(dirs)?;
        let mut model = Model::new(rows, cursor, notice.take(), arrangement);
        model.secondary = secondary;
        let action = {
            let mut screen = Screen::enter(&term)?;
            loop {
                let lines = render(
                    &model,
                    snapshot::now(),
                    &agent_line(dirs),
                    terminal_width(&term),
                );
                screen.draw(&lines)?;
                let key = term.read_key()?;
                screen.clear_frame(lines.len())?;
                model.notice = None;
                match model.handle(key) {
                    Action::Continue => continue,
                    // Saved on the spot, so the next `show` and the next
                    // visit draw the table the same way.
                    Action::CycleSort => {
                        let saved = accounts::set_table_sort(dirs, model.arrangement.sort);
                        if let Err(error) = saved {
                            model.notice = Some(error.to_string());
                        }
                        continue;
                    }
                    Action::ToggleGroup => {
                        let saved = accounts::set_table_grouped(dirs, model.arrangement.grouped);
                        if let Err(error) = saved {
                            model.notice = Some(error.to_string());
                        }
                        continue;
                    }
                    // The two quick questions are asked in place; everything
                    // else needs the plain terminal.
                    Action::Remove => {
                        let row = model.current().expect("a row under the cursor");
                        let what = if row.account.owns_home() {
                            "and its sign-in"
                        } else {
                            "(its home stays)"
                        };
                        let question = format!("Remove {} {what}?", row.account.display());
                        if !confirm_inline(&term, &question)? {
                            continue;
                        }
                        screen.finish()?;
                        break Action::Remove;
                    }
                    Action::Rename => {
                        let row = model.current().expect("a row under the cursor");
                        let current = row.account.label.clone().unwrap_or_default();
                        let answer =
                            read_line_inline(&term, "New label (empty keeps it):", &current)?;
                        match answer {
                            Some(label) if label != current => {
                                match accounts::rename(dirs, &row.account.id, Some(label)) {
                                    Ok(account) => {
                                        model.notice =
                                            Some(format!("now called {}", account.display()));
                                        screen.finish()?;
                                        break Action::Continue;
                                    }
                                    Err(error) => model.notice = Some(error.to_string()),
                                }
                            }
                            _ => {}
                        }
                        continue;
                    }
                    // Switching the poller OFF is asked in place too. On 15
                    // September 2026 a stray `b` — next to `a` and `e`, both
                    // harmless — removed the laptop's scheduler with one dim
                    // line of feedback, and nothing polled for three days
                    // before anyone noticed. Switching it on stays a single
                    // key: the worst case there is a poll.
                    Action::ToggleAgent if agent::is_installed() => {
                        if !confirm_inline(
                            &term,
                            "Switch the background poller off? Nothing polls until it is on again.",
                        )? {
                            continue;
                        }
                        screen.finish()?;
                        break Action::ToggleAgent;
                    }
                    other => {
                        screen.finish()?;
                        break other;
                    }
                }
            }
        };
        cursor = model.cursor;
        secondary = model.secondary;
        notice = model.notice.take();
        match action {
            Action::Quit => return Ok(()),
            Action::Continue => {}
            Action::Add => notice = add_flow(&term, dirs)?,
            Action::Login => {
                let account = model.current().expect("a row").account.clone();
                notice = Some(login_flow(&term, dirs, &account));
            }
            Action::Remove => {
                let account = model.current().expect("a row").account.clone();
                let (removed, fate) = accounts::remove(dirs, &account.id)?;
                notice = Some(match fate {
                    HomeFate::Deleted(_) => format!("removed {} and its home", removed.display()),
                    HomeFate::Kept(home) => {
                        format!(
                            "removed {} — home left at {}",
                            removed.display(),
                            display(&home)
                        )
                    }
                });
            }
            Action::Details => {
                let row = model.current().expect("a row");
                notice = details_flow(&term, dirs, row)?;
            }
            Action::PollAll => notice = Some(poll_flow(&term, dirs, Selection::default())),
            Action::ToggleAgent => notice = Some(toggle_agent(dirs)),
            Action::Events => events_flow(&term, dirs)?,
            Action::CycleInterval => notice = Some(cycle_interval(dirs)),
            Action::Rename => unreachable!("rename completes in place"),
            Action::CycleSort | Action::ToggleGroup => {
                unreachable!("the arrangement is saved in place")
            }
        }
    }
}

/// Every account with its latest snapshot, and how the table is arranged.
fn load_rows(dirs: &LimitsDirs) -> Result<(Vec<Row>, Arrangement)> {
    let registry = Registry::load_or_default(&dirs.config_file())?;
    let arrangement = registry.arrangement();
    let snapshots = snapshot::load_snapshots(dirs)?;
    let rows = registry
        .accounts
        .into_iter()
        .map(|account| Row {
            signed_in: account.signed_in(dirs),
            snapshot: snapshots
                .iter()
                .find(|s| s.provider == account.provider && s.id == account.id)
                .cloned(),
            account,
        })
        .collect();
    Ok((rows, arrangement))
}

/// Which product, what label, then the sign-in. Escape at the first question
/// adds nothing.
fn add_flow(term: &Term, dirs: &LimitsDirs) -> Result<Option<String>> {
    let Some(provider) = ask_provider()? else {
        return Ok(None);
    };
    let label = ask_label(None)?;
    let account = accounts::add(
        dirs,
        provider,
        AddOptions {
            label,
            ..AddOptions::default()
        },
    )?;
    term.write_line(&format!(
        "  {} added {}  home: {}",
        style("✓").green(),
        account.display(),
        display(&account.poll_home(dirs))
    ))?;
    if !ask_yes_no("Sign in now?")? {
        return Ok(Some(format!(
            "added {} — l signs it in when you are ready",
            account.display()
        )));
    }
    Ok(Some(login_flow(term, dirs, &account)))
}

/// The product's own sign-in, narrated on the plain terminal.
fn login_flow(term: &Term, dirs: &LimitsDirs, account: &Account) -> String {
    let outcome = match accounts::login(dirs, account) {
        Ok(()) => format!("{} signed in — enter polls it", account.display()),
        Err(error) => format!("{} not signed in: {error}", account.display()),
    };
    let _ = term.write_line(&format!("  {} {outcome}", style("→").dim()));
    let _ = press_any_key(term, "press any key to return");
    outcome
}

/// One account in full on the plain terminal, every window and detail.
/// Enter polls it from there; any other key goes back to the list.
fn details_flow(term: &Term, dirs: &LimitsDirs, row: &Row) -> Result<Option<String>> {
    term.write_line("")?;
    match &row.snapshot {
        Some(snapshot) => {
            let stale = if row.signed_in {
                Vec::new()
            } else {
                vec![snapshot.display()]
            };
            let plan = row.account.plan.as_ref();
            for line in show::render_account(snapshot, snapshot::now(), &stale, plan).lines() {
                term.write_line(line)?;
            }
        }
        None => {
            term.write_line(&format!(
                "  {}  {}",
                style(row.account.display()).bold(),
                style("never polled").dim()
            ))?;
            if let Some(plan) = &row.account.plan {
                let today = table::local_day(snapshot::now());
                let line = format!("plan: {}", show::plan_line(plan, today));
                term.write_line(&format!("    {}", style(line).dim()))?;
            }
        }
    }
    term.write_line("")?;
    term.write_line(&format!(
        "  {}",
        style("enter polls it now · any other key returns").dim()
    ))?;
    if term.read_key()? != Key::Enter {
        return Ok(None);
    }
    let selection = Selection {
        account: Some(row.account.id.clone()),
        ..Selection::default()
    };
    Ok(Some(poll_flow(term, dirs, selection)))
}

/// A poll, narrated on the plain terminal so each account's line is visible
/// as it lands.
fn poll_flow(term: &Term, dirs: &LimitsDirs, selection: Selection) -> String {
    let outcome = match accounts::require(dirs)
        .and_then(|registry| accounts::poll(dirs, &registry, &selection))
    {
        Ok(polled) if polled.failures == 0 => format!("polled {} account(s)", polled.polled),
        Ok(polled) => format!(
            "polled {} account(s), {} failed — see the table",
            polled.polled, polled.failures
        ),
        Err(error) => format!("poll failed: {error}"),
    };
    let _ = press_any_key(term, "press any key to return");
    outcome
}

fn toggle_agent(dirs: &LimitsDirs) -> String {
    if agent::is_installed() {
        return match agent::uninstall() {
            Ok(_) => "background poller off".into(),
            Err(error) => format!("could not remove the poller: {error}"),
        };
    }
    match accounts::require(dirs).and_then(|_| agent::install(dirs)) {
        Ok(_) => format!(
            "background poller on — every {} min, a Claude turn only when due",
            agent::TICK_SECONDS / 60
        ),
        Err(error) => format!("could not install the poller: {error}"),
    }
}

fn agent_line(dirs: &LimitsDirs) -> String {
    let interval = Registry::load_or_default(&dirs.config_file())
        .map(|r| r.interval_minutes)
        .unwrap_or(crate::registry::DEFAULT_INTERVAL_MINUTES);
    let claude = format!("Claude every {}", crate::registry::interval_label(interval));
    if agent::is_installed() {
        format!("agent on · {claude}")
    } else {
        format!("agent off · {claude}")
    }
}

/// The last twenty events, on the plain terminal.
fn events_flow(term: &Term, dirs: &LimitsDirs) -> Result<()> {
    let now = snapshot::now();
    let recent = crate::events::recent(dirs, 20)?;
    term.write_line("")?;
    if recent.is_empty() {
        term.write_line(&format!("  {}", style("no events yet").dim()))?;
    }
    for event in &recent {
        term.write_line(&format!("  {}", crate::events::line(event, now)))?;
    }
    press_any_key(term, "press any key to return")?;
    Ok(())
}

/// 15m → 30m → 1h → 2h → 15m.
fn cycle_interval(dirs: &LimitsDirs) -> String {
    const STEPS: [u64; 4] = [15, 30, 60, 120];
    let current = Registry::load_or_default(&dirs.config_file())
        .map(|r| r.interval_minutes)
        .unwrap_or(crate::registry::DEFAULT_INTERVAL_MINUTES);
    let next = STEPS
        .iter()
        .copied()
        .find(|step| *step > current)
        .unwrap_or(STEPS[0]);
    match accounts::set_interval(dirs, next) {
        Ok(_) => format!(
            "a Claude turn at most every {}",
            crate::registry::interval_label(next)
        ),
        Err(error) => error.to_string(),
    }
}

// ── prompts shared with the command line ─────────────────────────────────

/// Which product? A list, not a guess: a wrong answer would send a poll at
/// the wrong product.
pub fn ask_provider() -> Result<Option<Provider>> {
    let term = Term::stderr();
    let items: Vec<String> = Provider::ALL
        .iter()
        .map(|p| format!("  {:<7} {}", p.to_string(), style(p.display_name()).dim()))
        .collect();
    let picked = select_one(
        &term,
        "Which product?",
        &items,
        &mut Keys::User,
        Footer::Quit,
    )?;
    Ok(picked.map(|index| Provider::ALL[index]))
}

/// A label, or nothing. Optional and free to change later, so the prompt
/// says so.
pub fn ask_label(initial: Option<&str>) -> Result<Option<String>> {
    let term = Term::stderr();
    let answer = read_line_inline(
        &term,
        "A label for it? Optional, and you can change it later:",
        initial.unwrap_or_default(),
    )?;
    if let Some(label) = &answer {
        crate::registry::validate_label(label)?;
    }
    Ok(answer)
}

/// `[Y/n]` on its own line; Enter means yes.
pub fn ask_yes_no(question: &str) -> Result<bool> {
    let term = Term::stderr();
    term.write_str(&format!(
        "  {} {question} {} ",
        style("?").yellow().bold(),
        style("[Y/n]").dim()
    ))?;
    let answer = term.read_line()?;
    Ok(matches!(
        answer.trim().to_ascii_lowercase().as_str(),
        "" | "y" | "yes"
    ))
}

// ── rendering ────────────────────────────────────────────────────────────

/// Below this width the footer uses its short words.
const COMPACT_WIDTH: usize = 100;
/// The title's margin, and the least gap between the title and the clock.
const HEADER_MARGIN: usize = 2;

/// Pure, so the whole frame is testable against a fixed clock.
fn render(model: &Model, now: u64, agent: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    lines.push(header(model.rows.len(), now, agent, width));
    lines.push(String::new());

    if model.rows.is_empty() {
        for text in [
            "Each account signs in once, into a home only ovm limits uses — never",
            "~/.claude or ~/.codex. A Claude poll is one real turn on the cheapest",
            "model, a few cents of quota; Codex is asked over app-server and costs",
            "nothing. No credential is read, copied, or sent.",
        ] {
            lines.push(format!("  {}", style(text).dim()));
        }
    } else {
        let table_lines: Vec<Line> = model.rows.iter().map(table_line).collect();
        lines.extend(table::render(
            &table_lines,
            &table::Options {
                now,
                width,
                secondary: model.secondary,
                cursor: Some(model.cursor),
                grouped: model.arrangement.grouped,
            },
        ));
    }

    lines.push(String::new());
    if let Some(notice) = &model.notice {
        lines.push(format!("  {} {notice}", style("!").yellow().bold()));
        lines.push(String::new());
    }
    lines.push(footer(&hints(model, width)));
    lines
}

/// `ovm limits — 6 accounts` on the left, `Fri 02 Oct 13:26 · agent off ·
/// Claude every 15m` on the right. The count goes when the line is too
/// narrow for both.
fn header(count: usize, now: u64, agent: &str, width: usize) -> String {
    let right = format!("{} · {agent}", show::local_stamp(now));
    let full = match count {
        0 => "ovm limits — no accounts yet".to_string(),
        1 => "ovm limits — 1 account".to_string(),
        n => format!("ovm limits — {n} accounts"),
    };
    let room = width.saturating_sub(HEADER_MARGIN * 2);
    let measure = console::measure_text_width;
    let title = if count > 0 && measure(&full) + HEADER_MARGIN + measure(&right) > room {
        "ovm limits".to_string()
    } else {
        full
    };
    let gap = room
        .saturating_sub(measure(&title) + measure(&right))
        .max(HEADER_MARGIN);
    format!(
        "  {}{}{}",
        style(title).bold(),
        " ".repeat(gap),
        style(right).dim()
    )
}

fn hints(model: &Model, width: usize) -> Vec<(&'static str, &'static str)> {
    if model.rows.is_empty() {
        return vec![("a", "add"), ("b", "agent"), ("q", "quit")];
    }
    let windows = if model.secondary {
        "hide 5h"
    } else {
        "show 5h"
    };
    let sort = match model.arrangement.sort {
        table::SortOrder::Provider => "sort: provider",
        table::SortOrder::Reset => "sort: reset",
    };
    let group = if model.arrangement.grouped {
        "group: on"
    } else {
        "group: off"
    };
    if width < COMPACT_WIDTH {
        return vec![
            ("↑↓", "move"),
            ("enter", "details"),
            ("a", "add"),
            ("l", "login"),
            ("r", "rename"),
            ("d", "remove"),
            ("p", "poll all"),
            ("w", windows),
            ("s", sort),
            ("g", group),
            ("e", "events"),
            ("i", "interval"),
            ("b", "agent"),
            ("q", "quit"),
        ];
    }
    vec![
        ("↑↓", "navigate"),
        ("enter", "details"),
        ("a", "add"),
        ("l", "sign in"),
        ("r", "rename"),
        ("d", "remove"),
        ("p", "poll all"),
        ("w", windows),
        ("s", sort),
        ("g", group),
        ("e", "events"),
        ("i", "interval"),
        ("b", "agent on/off"),
        ("q", "quit"),
    ]
}

/// The snapshot whose numbers the row shows, if it shows any.
fn usage_snapshot(row: &Row) -> Option<&AccountSnapshot> {
    row.snapshot
        .as_ref()
        .filter(|s| row.signed_in && s.error.is_none() && !s.windows.is_empty())
}

/// One account as a table row. One state at a time, in the order a person
/// would want to know: not signed in, never polled, failed, numbers.
fn table_line(row: &Row) -> Line<'_> {
    let account = &row.account;
    let cells = if !row.signed_in && account.live_only {
        Cells::Message("not signed in — sign in inside its home")
    } else if !row.signed_in {
        Cells::Message("not signed in — l to sign in")
    } else {
        match &row.snapshot {
            None if account.live_only => Cells::Message("live-only — waiting for a session"),
            None => Cells::Message("never polled — p to poll"),
            Some(snapshot) => match &snapshot.error {
                Some(error) => Cells::Error(error),
                None if snapshot.windows.is_empty() => Cells::Message("no windows reported"),
                None => Cells::Usage(snapshot),
            },
        }
    };
    Line {
        name: account.label.as_deref().unwrap_or(&account.id),
        provider: account.provider,
        cells,
        plan: account.plan.as_ref(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::Window;

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).trim_end().to_string())
            .collect()
    }

    fn window(label: &str, used: f64) -> Window {
        Window {
            id: label.into(),
            label: label.into(),
            used_percent: used,
            resets_at: None,
            // Real lengths, so the row proves the long window leads.
            window_minutes: match label {
                "5h" => Some(300),
                "7d" => Some(7 * 24 * 60),
                _ => None,
            },
            last_reset_at: None,
        }
    }

    fn resetting(label: &str, used: f64, resets_at: u64) -> Window {
        Window {
            resets_at: Some(resets_at),
            ..window(label, used)
        }
    }

    fn polled(account: &Account, windows: Vec<Window>, error: Option<&str>) -> AccountSnapshot {
        AccountSnapshot {
            captured_at: 1_000_000 - 2 * 3600,
            host: "m5".into(),
            windows,
            error: error.map(String::from),
            ..AccountSnapshot::for_account(account, "test")
        }
    }

    fn rows() -> Vec<Row> {
        let simcity = Account::new(Provider::Claude, "claude-1", Some("simcity".into()));
        let mochi = Account::new(Provider::Claude, "claude-2", Some("mochi".into()));
        let codex = Account::new(Provider::Codex, "codex-1", None);
        let spare = Account::new(Provider::Codex, "codex-2", Some("spare".into()));
        vec![
            Row {
                snapshot: Some(polled(
                    &simcity,
                    vec![
                        resetting("5h", 66.0, 1_000_000 + 4 * 3600 + 10 * 60),
                        window("7d", 91.0),
                    ],
                    None,
                )),
                signed_in: true,
                account: simcity,
            },
            Row {
                snapshot: Some(polled(&mochi, vec![], Some("signed out"))),
                signed_in: true,
                account: mochi,
            },
            Row {
                snapshot: None,
                signed_in: true,
                account: codex,
            },
            Row {
                snapshot: None,
                signed_in: false,
                account: spare,
            },
        ]
    }

    #[test]
    fn the_table_shows_one_state_per_row_and_marks_the_cursor() {
        console::set_colors_enabled(false);
        let model = Model::new(rows(), 1, None, Arrangement::default());
        let lines = plain(&render(&model, 1_000_000, "agent off", 120));
        assert!(
            lines[1].starts_with("  ovm limits — 4 accounts  "),
            "{}",
            lines[1]
        );
        assert!(
            lines[1].ends_with(&format!("{} · agent off", show::local_stamp(1_000_000))),
            "{}",
            lines[1]
        );
        assert_eq!(lines[3], "                      left   resets back");
        assert!(
            lines[4].starts_with("  simcity    claude     9%   —"),
            "{}",
            lines[4]
        );
        assert!(!lines[4].contains("66%"), "the 5h window is hidden");
        assert_eq!(lines[5], "› mochi      claude   ✗ signed out");
        assert_eq!(lines[6], "  codex-1    codex    never polled — p to poll");
        assert_eq!(
            lines[7],
            "  spare      codex    not signed in — l to sign in"
        );
        let footer = lines.last().unwrap();
        assert!(footer.contains("enter details"), "{footer}");
        assert!(footer.contains("w show 5h"), "{footer}");
        assert!(footer.contains("d remove"), "{footer}");
    }

    /// Two Claude accounts and a Codex one whose reset comes first of all.
    fn resetting_rows() -> Vec<Row> {
        let late = Account::new(Provider::Claude, "claude-1", Some("late".into()));
        let soon = Account::new(Provider::Claude, "claude-2", Some("soon".into()));
        let codex = Account::new(Provider::Codex, "codex-1", Some("first".into()));
        vec![
            Row {
                snapshot: Some(polled(
                    &codex,
                    vec![resetting("codex 1w", 10.0, 1_000_000 + 60)],
                    None,
                )),
                signed_in: true,
                account: codex,
            },
            Row {
                snapshot: Some(polled(
                    &late,
                    vec![resetting("7d", 10.0, 1_000_000 + 5 * 86_400)],
                    None,
                )),
                signed_in: true,
                account: late,
            },
            Row {
                snapshot: Some(polled(
                    &soon,
                    vec![
                        resetting("5h", 10.0, 1_000_000 + 30),
                        resetting("7d", 10.0, 1_000_000 + 2 * 86_400),
                    ],
                    None,
                )),
                signed_in: true,
                account: soon,
            },
        ]
    }

    fn labels(model: &Model) -> Vec<&str> {
        model
            .rows
            .iter()
            .map(|row| row.account.label.as_deref().unwrap())
            .collect()
    }

    #[test]
    fn rows_sort_by_provider_then_soonest_reset() {
        let model = Model::new(resetting_rows(), 0, None, Arrangement::default());
        assert_eq!(labels(&model), ["soon", "late", "first"]);
    }

    #[test]
    fn s_cycles_the_sort_and_the_cursor_stays_on_its_account() {
        console::set_colors_enabled(false);
        let mut model = Model::new(resetting_rows(), 1, None, Arrangement::default());
        assert_eq!(model.current().unwrap().account.id, "claude-1");
        let footer = plain(&render(&model, 1_000_000, "agent off", 120));
        assert!(
            footer.last().unwrap().contains("s sort: provider"),
            "{footer:#?}"
        );
        assert!(
            footer.last().unwrap().contains("g group: off"),
            "{footer:#?}"
        );

        assert_eq!(model.handle(Key::Char('s')), Action::CycleSort);
        assert_eq!(model.arrangement.sort, table::SortOrder::Reset);
        assert_eq!(labels(&model), ["first", "soon", "late"]);
        assert_eq!(model.current().unwrap().account.id, "claude-1");
        let lines = plain(&render(&model, 1_000_000, "agent off", 120));
        assert!(
            lines.last().unwrap().contains("s sort: reset"),
            "{lines:#?}"
        );

        assert_eq!(model.handle(Key::Char('s')), Action::CycleSort);
        assert_eq!(model.arrangement.sort, table::SortOrder::Provider);
        assert_eq!(labels(&model), ["soon", "late", "first"]);
    }

    #[test]
    fn g_toggles_a_block_per_provider() {
        console::set_colors_enabled(false);
        let reset_first = Arrangement {
            sort: table::SortOrder::Reset,
            grouped: false,
        };
        let mut model = Model::new(resetting_rows(), 0, None, reset_first);
        assert_eq!(labels(&model), ["first", "soon", "late"]);

        assert_eq!(model.handle(Key::Char('g')), Action::ToggleGroup);
        assert!(model.arrangement.grouped);
        assert_eq!(labels(&model), ["soon", "late", "first"]);
        let lines = plain(&render(&model, 1_000_000, "agent off", 120));
        assert!(lines[3].starts_with("  Claude Code "), "{lines:#?}");
        assert!(lines[4].contains("soon"), "{lines:#?}");
        assert!(lines[5].contains("late"), "{lines:#?}");
        assert_eq!(lines[6], "");
        assert!(lines[7].starts_with("  Codex "), "{lines:#?}");
        assert!(lines[8].contains("first"), "{lines:#?}");
        assert!(lines.last().unwrap().contains("g group: on"), "{lines:#?}");

        assert_eq!(model.handle(Key::Char('g')), Action::ToggleGroup);
        assert!(!model.arrangement.grouped);
        assert_eq!(labels(&model), ["first", "soon", "late"]);
    }

    #[test]
    fn w_shows_and_hides_the_secondary_windows() {
        console::set_colors_enabled(false);
        let mut model = Model::new(rows(), 0, None, Arrangement::default());
        let before = plain(&render(&model, 1_000_000, "agent off", 120));
        assert!(!before.iter().any(|l| l.contains("66%")), "{before:#?}");

        assert_eq!(model.handle(Key::Char('w')), Action::Continue);
        assert!(model.secondary);
        let after = plain(&render(&model, 1_000_000, "agent off", 120));
        assert_eq!(after.len(), before.len() + 1, "{after:#?}");
        assert!(
            after[5].starts_with("    5h                 34%   "),
            "{}",
            after[5]
        );
        assert!(after[5].ends_with("in 4h 10m"), "{}", after[5]);
        assert!(after.last().unwrap().contains("w hide 5h"), "{after:#?}");

        model.handle(Key::Char('w'));
        assert!(!model.secondary);
    }

    #[test]
    fn a_narrow_header_drops_the_account_count() {
        console::set_colors_enabled(false);
        let line = plain(&[header(4, 1_000_000, "agent off · Claude every 15m", 60)]).remove(0);
        assert!(line.starts_with("  ovm limits  "), "{line}");
        assert!(!line.contains("accounts"), "{line}");
        assert!(line.ends_with("agent off · Claude every 15m"), "{line}");
    }

    #[test]
    fn an_empty_registry_explains_itself_and_offers_only_add() {
        console::set_colors_enabled(false);
        let model = Model::new(Vec::new(), 0, None, Arrangement::default());
        let lines = plain(&render(&model, 0, "agent off", 80));
        assert!(
            lines[1].starts_with("  ovm limits — no accounts yet  "),
            "{}",
            lines[1]
        );
        assert!(lines[1].ends_with("agent off"), "{}", lines[1]);
        assert!(lines.iter().any(|l| l.contains("never")), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  a add · b agent · q quit");
    }

    #[test]
    fn a_notice_sits_between_the_table_and_the_footer() {
        console::set_colors_enabled(false);
        let model = Model::new(
            rows(),
            0,
            Some("polled 4 account(s)".into()),
            Arrangement::default(),
        );
        let lines = plain(&render(&model, 1_000_000, "agent off", 80));
        let footer_index = lines.len() - 1;
        assert_eq!(lines[footer_index - 2], "  ! polled 4 account(s)");
    }

    #[test]
    fn keys_map_to_actions_and_row_actions_need_a_row() {
        let mut model = Model::new(rows(), 0, None, Arrangement::default());
        assert_eq!(model.handle(Key::ArrowDown), Action::Continue);
        assert_eq!(model.cursor, 1);
        assert_eq!(model.handle(Key::Char('k')), Action::Continue);
        assert_eq!(model.cursor, 0);
        assert_eq!(model.handle(Key::ArrowUp), Action::Continue);
        assert_eq!(model.cursor, 0, "stays put at the top");
        assert_eq!(model.handle(Key::Enter), Action::Details);
        assert_eq!(model.handle(Key::Char('l')), Action::Login);
        assert_eq!(model.handle(Key::Char('r')), Action::Rename);
        assert_eq!(model.handle(Key::Char('d')), Action::Remove);
        assert_eq!(model.handle(Key::Char('p')), Action::PollAll);
        assert_eq!(model.handle(Key::Char('b')), Action::ToggleAgent);
        assert_eq!(model.handle(Key::Char('e')), Action::Events);
        assert_eq!(model.handle(Key::Char('i')), Action::CycleInterval);
        assert_eq!(model.handle(Key::Char('w')), Action::Continue);
        assert_eq!(model.handle(Key::Char('a')), Action::Add);
        assert_eq!(model.handle(Key::Escape), Action::Quit);
        assert_eq!(model.handle(Key::Char('x')), Action::Continue);

        let mut empty = Model::new(Vec::new(), 5, None, Arrangement::default());
        assert_eq!(empty.cursor, 0, "clamped");
        assert_eq!(empty.handle(Key::Enter), Action::Continue);
        assert_eq!(empty.handle(Key::Char('d')), Action::Continue);
        assert_eq!(empty.handle(Key::Char('a')), Action::Add);
        assert_eq!(empty.handle(Key::Char('q')), Action::Quit);
    }

    #[test]
    fn the_cursor_survives_a_row_disappearing() {
        let model = Model::new(
            rows().into_iter().take(2).collect(),
            3,
            None,
            Arrangement::default(),
        );
        assert_eq!(model.cursor, 1);
    }
}
