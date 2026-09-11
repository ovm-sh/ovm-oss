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
use crate::{agent, show, Result};
use ovm_tui::{
    confirm_inline, fixed_width_cell, footer, press_any_key, read_line_inline, select_one, style,
    terminal_width, Footer, Key, Keys, Screen, Term,
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
    PollOne,
    PollAll,
    ToggleAgent,
    Events,
    CycleInterval,
    Quit,
}

struct Model {
    rows: Vec<Row>,
    cursor: usize,
    /// One-shot line under the table, so a key that did something says so.
    notice: Option<String>,
}

impl Model {
    fn new(rows: Vec<Row>, cursor: usize, notice: Option<String>) -> Self {
        let cursor = cursor.min(rows.len().saturating_sub(1));
        Self {
            rows,
            cursor,
            notice,
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
            Key::Escape | Key::Char('q') | Key::Char('Q') => Action::Quit,
            Key::Enter if has_rows => Action::PollOne,
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
    loop {
        let mut model = Model::new(load_rows(dirs)?, cursor, notice.take());
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
                    other => {
                        screen.finish()?;
                        break other;
                    }
                }
            }
        };
        cursor = model.cursor;
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
            Action::PollOne => {
                let account = model.current().expect("a row").account.clone();
                notice = Some(poll_flow(
                    &term,
                    dirs,
                    Selection {
                        account: Some(account.id),
                        ..Selection::default()
                    },
                ));
            }
            Action::PollAll => notice = Some(poll_flow(&term, dirs, Selection::default())),
            Action::ToggleAgent => notice = Some(toggle_agent(dirs)),
            Action::Events => events_flow(&term, dirs)?,
            Action::CycleInterval => notice = Some(cycle_interval(dirs)),
            Action::Rename => unreachable!("rename completes in place"),
        }
    }
}

fn load_rows(dirs: &LimitsDirs) -> Result<Vec<Row>> {
    let registry = Registry::load_or_default(&dirs.config_file())?;
    let snapshots = snapshot::load_snapshots(dirs)?;
    Ok(registry
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
        .collect())
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

const MIN_ID_WIDTH: usize = 8;
const MIN_LABEL_WIDTH: usize = 5;
const MAX_LABEL_WIDTH: usize = 20;
const POLLED_WIDTH: usize = 10;
const MIN_USAGE_WIDTH: usize = 12;
/// Cursor cell, the three fixed gaps, and a margin.
const ROW_CHROME_WIDTH: usize = 2 + 3 * 2 + 1;

/// Pure, so the whole frame is testable against a fixed clock.
fn render(model: &Model, now: u64, agent: &str, width: usize) -> Vec<String> {
    let mut lines = vec![String::new()];
    let count = model.rows.len();
    let heading = match count {
        0 => "ovm limits — no accounts yet".to_string(),
        1 => "ovm limits — 1 account".to_string(),
        n => format!("ovm limits — {n} accounts"),
    };
    lines.push(format!(
        "  {}  {}",
        style(heading).bold(),
        style(agent).dim()
    ));
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
        let id_width = model
            .rows
            .iter()
            .map(|row| row.account.id.chars().count())
            .max()
            .unwrap_or(0)
            .max(MIN_ID_WIDTH);
        let label_width = model
            .rows
            .iter()
            .filter_map(|row| row.account.label.as_ref())
            .map(|label| label.chars().count())
            .max()
            .unwrap_or(0)
            .clamp(MIN_LABEL_WIDTH, MAX_LABEL_WIDTH);
        let usage_width = width
            .saturating_sub(ROW_CHROME_WIDTH + id_width + label_width + POLLED_WIDTH)
            .max(MIN_USAGE_WIDTH);
        lines.push(format!(
            "    {}  {}  {}  {}",
            style(format!("{:<id_width$}", "id")).dim(),
            style(format!("{:<label_width$}", "label")).dim(),
            style(format!("{:<usage_width$}", "usage")).dim(),
            style("polled").dim()
        ));
        for (index, row) in model.rows.iter().enumerate() {
            let cells = render_row(row, now, id_width, label_width, usage_width);
            if index == model.cursor {
                lines.push(format!("{}   {cells}", style("›").cyan().bold()));
            } else {
                lines.push(format!("    {cells}"));
            }
        }
    }

    lines.push(String::new());
    if let Some(notice) = &model.notice {
        lines.push(format!("  {} {notice}", style("!").yellow().bold()));
        lines.push(String::new());
    }
    let compact = width < 100;
    let hints: Vec<(&str, &str)> = if model.rows.is_empty() {
        vec![("a", "add"), ("b", "agent"), ("q", "quit")]
    } else if compact {
        vec![
            ("↑↓", "move"),
            ("enter", "poll"),
            ("a", "add"),
            ("l", "login"),
            ("r", "rename"),
            ("d", "remove"),
            ("p", "poll all"),
            ("e", "events"),
            ("i", "interval"),
            ("b", "agent"),
            ("q", "quit"),
        ]
    } else {
        vec![
            ("↑↓", "navigate"),
            ("enter", "poll this one"),
            ("a", "add"),
            ("l", "sign in"),
            ("r", "rename"),
            ("d", "remove"),
            ("p", "poll all"),
            ("e", "events"),
            ("i", "interval"),
            ("b", "agent on/off"),
            ("q", "quit"),
        ]
    };
    lines.push(footer(&hints));
    lines
}

fn render_row(
    row: &Row,
    now: u64,
    id_width: usize,
    label_width: usize,
    usage_width: usize,
) -> String {
    let id = fixed_width_cell(&row.account.id, id_width);
    let label = match &row.account.label {
        Some(label) => fixed_width_cell(label, label_width),
        None => style(fixed_width_cell("—", label_width)).dim().to_string(),
    };
    let (usage, polled) = usage_cells(row, now, usage_width);
    format!("{id}  {label}  {usage}  {polled}")
}

/// The usage column and the age beside it. One state at a time, in the
/// order a person would want to know: not signed in, failed, never polled,
/// numbers.
fn usage_cells(row: &Row, now: u64, usage_width: usize) -> (String, String) {
    let dim = |text: &str| style(fixed_width_cell(text, usage_width)).dim().to_string();
    if !row.signed_in {
        return (dim("not signed in — l to sign in"), String::new());
    }
    let Some(snapshot) = &row.snapshot else {
        return (dim("never polled — enter to poll"), String::new());
    };
    let polled = style(show::human(now.saturating_sub(snapshot.captured_at)) + " ago")
        .dim()
        .to_string();
    if let Some(error) = &snapshot.error {
        let cell = fixed_width_cell(&format!("✗ {error}"), usage_width);
        return (style(cell).red().to_string(), polled);
    }
    if snapshot.windows.is_empty() {
        return (dim("no windows reported"), polled);
    }
    let text = snapshot
        .windows
        .iter()
        .map(|w| show::window_brief(w, now))
        .collect::<Vec<_>>()
        .join(" · ");
    let cell = fixed_width_cell(&text, usage_width);
    let hottest = snapshot
        .windows
        .iter()
        .map(|w| w.used_percent)
        .fold(0.0_f64, f64::max);
    let cell = if hottest >= 90.0 {
        style(cell).red().to_string()
    } else if hottest >= 70.0 {
        style(cell).yellow().to_string()
    } else {
        cell
    };
    (cell, polled)
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
            window_minutes: None,
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
        let model = Model::new(rows(), 1, None);
        let lines = plain(&render(&model, 1_000_000, "agent off", 120));
        assert_eq!(lines[1], "  ovm limits — 4 accounts  agent off");
        assert!(lines[3].starts_with("    id"), "{}", lines[3]);
        assert!(
            lines[4].starts_with("    claude-1  simcity  5h 66% (resets in 4h 10m) · 7d 91%"),
            "{}",
            lines[4]
        );
        assert!(lines[4].ends_with("2h 0m ago"), "{}", lines[4]);
        assert!(
            lines[5].starts_with("›   claude-2  mochi    ✗ signed out"),
            "{}",
            lines[5]
        );
        assert!(
            lines[6].contains("codex-1   —        never polled — enter to poll"),
            "{}",
            lines[6]
        );
        assert!(
            lines[7].contains("codex-2   spare    not signed in — l to sign in"),
            "{}",
            lines[7]
        );
        assert!(
            lines.last().unwrap().contains("d remove"),
            "{:?}",
            lines.last()
        );
    }

    #[test]
    fn an_empty_registry_explains_itself_and_offers_only_add() {
        console::set_colors_enabled(false);
        let model = Model::new(Vec::new(), 0, None);
        let lines = plain(&render(&model, 0, "agent off", 80));
        assert_eq!(lines[1], "  ovm limits — no accounts yet  agent off");
        assert!(lines.iter().any(|l| l.contains("never")), "{lines:?}");
        assert_eq!(lines.last().unwrap(), "  a add · b agent · q quit");
    }

    #[test]
    fn a_notice_sits_between_the_table_and_the_footer() {
        console::set_colors_enabled(false);
        let model = Model::new(rows(), 0, Some("polled 4 account(s)".into()));
        let lines = plain(&render(&model, 1_000_000, "agent off", 80));
        let footer_index = lines.len() - 1;
        assert_eq!(lines[footer_index - 2], "  ! polled 4 account(s)");
    }

    #[test]
    fn keys_map_to_actions_and_row_actions_need_a_row() {
        let mut model = Model::new(rows(), 0, None);
        assert_eq!(model.handle(Key::ArrowDown), Action::Continue);
        assert_eq!(model.cursor, 1);
        assert_eq!(model.handle(Key::Char('k')), Action::Continue);
        assert_eq!(model.cursor, 0);
        assert_eq!(model.handle(Key::ArrowUp), Action::Continue);
        assert_eq!(model.cursor, 0, "stays put at the top");
        assert_eq!(model.handle(Key::Enter), Action::PollOne);
        assert_eq!(model.handle(Key::Char('l')), Action::Login);
        assert_eq!(model.handle(Key::Char('r')), Action::Rename);
        assert_eq!(model.handle(Key::Char('d')), Action::Remove);
        assert_eq!(model.handle(Key::Char('p')), Action::PollAll);
        assert_eq!(model.handle(Key::Char('b')), Action::ToggleAgent);
        assert_eq!(model.handle(Key::Char('e')), Action::Events);
        assert_eq!(model.handle(Key::Char('i')), Action::CycleInterval);
        assert_eq!(model.handle(Key::Char('a')), Action::Add);
        assert_eq!(model.handle(Key::Escape), Action::Quit);
        assert_eq!(model.handle(Key::Char('x')), Action::Continue);

        let mut empty = Model::new(Vec::new(), 5, None);
        assert_eq!(empty.cursor, 0, "clamped");
        assert_eq!(empty.handle(Key::Enter), Action::Continue);
        assert_eq!(empty.handle(Key::Char('d')), Action::Continue);
        assert_eq!(empty.handle(Key::Char('a')), Action::Add);
        assert_eq!(empty.handle(Key::Char('q')), Action::Quit);
    }

    #[test]
    fn the_cursor_survives_a_row_disappearing() {
        let model = Model::new(rows().into_iter().take(2).collect(), 3, None);
        assert_eq!(model.cursor, 1);
    }
}
