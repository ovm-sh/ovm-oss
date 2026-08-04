//! The "which of these?" step of `ovm update`.
//!
//! A sweep that installs everything the moment you type it gives you one
//! choice: Ctrl-C, after it has already started. This asks first — and asks
//! only when there is a terminal to ask on, so scripts keep their old
//! behaviour (see `Mode` in [`super::update`]).
//!
//! Rendering follows the same primitives as the select TUI (`console::Term`
//! key loop, no dialoguer) so both pickers behave identically under a PTY.

use super::update::Available;
use crate::error::Result;
use crate::product::Product;
use console::{style, Key, Term};

/// Selection state: which rows are ticked, and where the cursor is. Pure, so
/// the key handling can be tested without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    checked: Vec<bool>,
    cursor: usize,
}

/// What a keypress asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Redraw and keep asking.
    Continue,
    /// Apply the ticked rows.
    Confirm,
    /// Change nothing.
    Cancel,
}

impl Selection {
    /// Everything starts ticked: the common answer is "yes, all of them", and
    /// the picker exists to let you say "except that one" — not to make the
    /// normal case cost N keystrokes.
    pub(crate) fn all(len: usize) -> Self {
        Self {
            checked: vec![true; len],
            cursor: 0,
        }
    }

    pub(crate) fn chosen(&self) -> Vec<usize> {
        self.checked
            .iter()
            .enumerate()
            .filter_map(|(index, checked)| checked.then_some(index))
            .collect()
    }

    pub(crate) fn count(&self) -> usize {
        self.checked.iter().filter(|checked| **checked).count()
    }

    pub(crate) fn cursor(&self) -> usize {
        self.cursor
    }

    pub(crate) fn is_checked(&self, index: usize) -> bool {
        self.checked.get(index).copied().unwrap_or(false)
    }

    /// Apply one key. Movement wraps, because a three-item list with a hard
    /// stop at each end is more annoying than either behaviour is subtle.
    pub(crate) fn handle(&mut self, key: &Key) -> Step {
        let len = self.checked.len();
        if len == 0 {
            return Step::Cancel;
        }
        match key {
            Key::ArrowUp | Key::Char('k') => {
                self.cursor = (self.cursor + len - 1) % len;
                Step::Continue
            }
            Key::ArrowDown | Key::Char('j') => {
                self.cursor = (self.cursor + 1) % len;
                Step::Continue
            }
            Key::Char(' ') | Key::Char('x') => {
                self.checked[self.cursor] = !self.checked[self.cursor];
                Step::Continue
            }
            Key::Char('a') => {
                self.checked.iter_mut().for_each(|checked| *checked = true);
                Step::Continue
            }
            Key::Char('n') => {
                self.checked.iter_mut().for_each(|checked| *checked = false);
                Step::Continue
            }
            Key::Enter => Step::Confirm,
            Key::Escape | Key::Char('q') => Step::Cancel,
            _ => Step::Continue,
        }
    }
}

/// Ask which updates to apply.
///
/// `Ok(None)` means the user cancelled — distinct from `Ok(Some(vec![]))`,
/// which is "I looked and chose none". Both leave the machine untouched, but
/// only one of them is worth a different closing line.
pub(crate) fn choose(available: &[(Product, &Available)]) -> Result<Option<Vec<usize>>> {
    let term = Term::stderr();
    let mut selection = Selection::all(available.len());

    term.hide_cursor()?;
    let result = run_loop(&term, available, &mut selection);
    term.show_cursor()?;
    // Leave the finished frame on screen: what you chose stays visible above
    // the installs that follow.
    result
}

fn run_loop(
    term: &Term,
    available: &[(Product, &Available)],
    selection: &mut Selection,
) -> Result<Option<Vec<usize>>> {
    let mut drawn = 0usize;
    loop {
        if drawn > 0 {
            term.clear_last_lines(drawn)?;
        }
        let frame = render(available, selection);
        for line in &frame {
            term.write_line(line)?;
        }
        drawn = frame.len();

        let key = term.read_key()?;
        match selection.handle(&key) {
            Step::Continue => {}
            Step::Confirm => return Ok(Some(selection.chosen())),
            Step::Cancel => return Ok(None),
        }
    }
}

/// Build the frame as lines, so tests can assert on what a user would see.
pub(crate) fn render(available: &[(Product, &Available)], selection: &Selection) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!(
            "  {}",
            style(format!("{} update(s) available", available.len())).bold()
        ),
    ];

    let width = available
        .iter()
        .map(|(product, _)| product.display_name().len())
        .max()
        .unwrap_or(0);

    for (index, (product, update)) in available.iter().enumerate() {
        let pointer = if index == selection.cursor() {
            "❯"
        } else {
            " "
        };
        let box_ = if selection.is_checked(index) {
            style("◉").green().to_string()
        } else {
            style("◯").dim().to_string()
        };
        let name = format!("{:<width$}", product.display_name(), width = width);
        lines.push(format!(
            "  {pointer} {box_} {name}  {} {} {}",
            style(&update.from).dim(),
            style("→").cyan(),
            style(&update.to).green().bold()
        ));
    }

    lines.push(String::new());
    lines.push(format!(
        "  {}",
        style(format!(
            "space toggle · a all · n none · enter update {} · esc cancel",
            selection.count()
        ))
        .dim()
    ));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn update(from: &str, to: &str) -> Available {
        Available {
            from: from.to_string(),
            to: to.to_string(),
        }
    }

    #[test]
    fn everything_starts_selected_so_the_common_answer_is_one_keypress() {
        let selection = Selection::all(3);
        assert_eq!(selection.chosen(), vec![0, 1, 2]);
        assert_eq!(selection.count(), 3);
    }

    #[test]
    fn space_deselects_the_row_under_the_cursor() {
        let mut selection = Selection::all(3);
        selection.handle(&Key::ArrowDown);
        selection.handle(&Key::Char(' '));

        assert_eq!(selection.chosen(), vec![0, 2]);
    }

    #[test]
    fn space_toggles_back_on() {
        let mut selection = Selection::all(2);
        selection.handle(&Key::Char(' '));
        selection.handle(&Key::Char(' '));

        assert_eq!(selection.chosen(), vec![0, 1]);
    }

    #[test]
    fn a_and_n_select_and_clear_everything() {
        let mut selection = Selection::all(3);
        assert_eq!(selection.handle(&Key::Char('n')), Step::Continue);
        assert!(selection.chosen().is_empty());
        selection.handle(&Key::Char('a'));
        assert_eq!(selection.chosen(), vec![0, 1, 2]);
    }

    #[test]
    fn the_cursor_wraps_at_both_ends() {
        let mut selection = Selection::all(3);
        selection.handle(&Key::ArrowUp);
        assert_eq!(selection.cursor(), 2, "up from the top wraps to the bottom");
        selection.handle(&Key::ArrowDown);
        assert_eq!(
            selection.cursor(),
            0,
            "down from the bottom wraps to the top"
        );
    }

    #[test]
    fn vim_keys_move_too() {
        let mut selection = Selection::all(3);
        selection.handle(&Key::Char('j'));
        assert_eq!(selection.cursor(), 1);
        selection.handle(&Key::Char('k'));
        assert_eq!(selection.cursor(), 0);
    }

    #[test]
    fn enter_confirms_and_escape_cancels() {
        let mut selection = Selection::all(2);
        assert_eq!(selection.handle(&Key::Enter), Step::Confirm);
        assert_eq!(selection.handle(&Key::Escape), Step::Cancel);
        assert_eq!(selection.handle(&Key::Char('q')), Step::Cancel);
    }

    #[test]
    fn unknown_keys_do_nothing_rather_than_confirming() {
        // A stray keypress must never be read as "yes, install".
        let mut selection = Selection::all(2);
        assert_eq!(selection.handle(&Key::Char('z')), Step::Continue);
        assert_eq!(selection.handle(&Key::Tab), Step::Continue);
        assert_eq!(selection.chosen(), vec![0, 1]);
    }

    #[test]
    fn confirming_with_nothing_ticked_is_an_empty_choice_not_everything() {
        let mut selection = Selection::all(2);
        selection.handle(&Key::Char('n'));
        assert_eq!(selection.handle(&Key::Enter), Step::Confirm);
        assert!(selection.chosen().is_empty());
    }

    #[test]
    fn the_frame_shows_every_update_and_the_live_count() {
        let claude = update("2.1.220", "2.1.221");
        let codex = update("rust-v0.145.0", "rust-v0.146.0");
        let available = vec![(Product::Claude, &claude), (Product::Codex, &codex)];
        let mut selection = Selection::all(2);
        selection.handle(&Key::Char(' ')); // untick the first

        let frame = render(&available, &selection).join("\n");

        assert!(frame.contains("2 update(s) available"));
        assert!(frame.contains("2.1.220") && frame.contains("2.1.221"));
        assert!(frame.contains("rust-v0.146.0"));
        assert!(
            frame.contains("enter update 1"),
            "the count must track the ticks: {frame}"
        );
    }
}
