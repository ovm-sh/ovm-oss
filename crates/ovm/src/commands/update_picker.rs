//! The "which of these?" step of `ovm update`.
//!
//! A sweep that installs everything the moment you type it gives you one
//! choice: Ctrl-C, after it has already started. This asks first — and asks
//! only when there is a terminal to ask on, so scripts keep their old
//! behaviour (see `Mode` in [`super::update`]).
//!
//! The same screen also carries the launch-time auto-update policies: the
//! bare interactive sweep is where you think about updating, so it is also
//! where the "should launches update by themselves?" knob lives. Update rows
//! are checkboxes; policy rows cycle `off / on / notify`. One enter applies
//! both, one escape abandons both.
//!
//! Rendering follows the same primitives as the select TUI (`console::Term`
//! key loop, no dialoguer) so both pickers behave identically under a PTY.

use super::update::Available;
use crate::config::AutoUpdatePolicy;
use crate::error::Result;
use crate::product::Product;
use console::{style, Key, Term};

/// Selection state: which update rows are ticked, where each policy row
/// currently points, and where the cursor is. The cursor runs over update
/// rows first, then policy rows. Pure, so the key handling can be tested
/// without a terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Selection {
    checked: Vec<bool>,
    policies: Vec<AutoUpdatePolicy>,
    cursor: usize,
}

/// What a keypress asked for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Step {
    /// Redraw and keep asking.
    Continue,
    /// Apply the ticked rows and any policy changes.
    Confirm,
    /// Change nothing.
    Cancel,
}

/// `off → on → notify → off`, or the reverse.
fn cycle_policy(policy: AutoUpdatePolicy, forward: bool) -> AutoUpdatePolicy {
    use AutoUpdatePolicy::{Notify, Off, On};
    match (policy, forward) {
        (Off, true) | (Notify, false) => On,
        (On, true) | (Off, false) => Notify,
        (Notify, true) | (On, false) => Off,
    }
}

impl Selection {
    /// Unpinned updates start ticked: the common answer is "yes, all of
    /// them", and the picker exists to let you say "except that one" — not to
    /// make the normal case cost N keystrokes. Pinned updates start unticked:
    /// the pin was deliberate, so moving it must be too.
    pub(crate) fn new(pinned: &[bool], policies: Vec<AutoUpdatePolicy>) -> Self {
        Self {
            checked: pinned.iter().map(|pinned| !pinned).collect(),
            policies,
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

    pub(crate) fn policies(&self) -> &[AutoUpdatePolicy] {
        &self.policies
    }

    /// The policy row the cursor is on, if it is on one.
    fn policy_under_cursor(&mut self) -> Option<&mut AutoUpdatePolicy> {
        self.policies
            .get_mut(self.cursor.wrapping_sub(self.checked.len()))
    }

    /// Apply one key. Movement wraps, because a short list with a hard stop
    /// at each end is more annoying than either behaviour is subtle.
    pub(crate) fn handle(&mut self, key: &Key) -> Step {
        let len = self.checked.len() + self.policies.len();
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
            // Space means "the obvious action for this row": tick an update,
            // advance a policy.
            Key::Char(' ') | Key::Char('x') => {
                if let Some(checked) = self.checked.get_mut(self.cursor) {
                    *checked = !*checked;
                } else if let Some(policy) = self.policy_under_cursor() {
                    *policy = cycle_policy(*policy, true);
                }
                Step::Continue
            }
            Key::ArrowRight | Key::Char('l') => {
                if let Some(policy) = self.policy_under_cursor() {
                    *policy = cycle_policy(*policy, true);
                }
                Step::Continue
            }
            Key::ArrowLeft | Key::Char('h') => {
                if let Some(policy) = self.policy_under_cursor() {
                    *policy = cycle_policy(*policy, false);
                }
                Step::Continue
            }
            // `a`/`n` speak about the update list only: bulk-flipping launch
            // policies is never what a "select all" gesture means.
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

/// Ask which updates to apply and where the launch policies should sit.
///
/// `Ok(None)` means the user cancelled — distinct from confirming an empty
/// choice. Both leave the machine untouched, but only one of them is worth a
/// different closing line. A confirm returns the ticked update indices and
/// the final policy row values (whether or not they moved; the caller diffs).
pub(crate) fn choose(
    available: &[(Product, &Available)],
    setting_names: &[&'static str],
    initial_policies: Vec<AutoUpdatePolicy>,
) -> Result<Option<(Vec<usize>, Vec<AutoUpdatePolicy>)>> {
    debug_assert_eq!(setting_names.len(), initial_policies.len());
    let pinned: Vec<bool> = available.iter().map(|(_, update)| update.pinned).collect();
    let mut selection = Selection::new(&pinned, initial_policies);

    let term = Term::stderr();
    term.hide_cursor()?;
    let result = run_loop(&term, available, setting_names, &mut selection);
    term.show_cursor()?;
    // Leave the finished frame on screen: what you chose stays visible above
    // the installs that follow.
    result
}

fn run_loop(
    term: &Term,
    available: &[(Product, &Available)],
    setting_names: &[&'static str],
    selection: &mut Selection,
) -> Result<Option<(Vec<usize>, Vec<AutoUpdatePolicy>)>> {
    let mut drawn = 0usize;
    loop {
        if drawn > 0 {
            term.clear_last_lines(drawn)?;
        }
        let frame = render(available, setting_names, selection);
        for line in &frame {
            term.write_line(line)?;
        }
        drawn = frame.len();

        let key = term.read_key()?;
        match selection.handle(&key) {
            Step::Continue => {}
            Step::Confirm => return Ok(Some((selection.chosen(), selection.policies().to_vec()))),
            Step::Cancel => return Ok(None),
        }
    }
}

/// Build the frame as lines, so tests can assert on what a user would see.
pub(crate) fn render(
    available: &[(Product, &Available)],
    setting_names: &[&str],
    selection: &Selection,
) -> Vec<String> {
    let mut lines = vec![String::new()];

    // One name column across both sections, so the screen reads as a table.
    let width = available
        .iter()
        .map(|(product, _)| product.display_name().len())
        .chain(setting_names.iter().map(|name| name.len()))
        .max()
        .unwrap_or(0);

    if !available.is_empty() {
        lines.push(format!(
            "  {}",
            style(format!("{} update(s) available", available.len())).bold()
        ));
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
            let mut line = format!(
                "  {pointer} {box_} {name}  {} {} {}",
                style(&update.from).dim(),
                style("→").cyan(),
                style(&update.to).green().bold()
            );
            if update.pinned {
                line.push_str(&format!("  {}", style("(pinned)").dim()));
            }
            lines.push(line);
        }
    }

    if !setting_names.is_empty() {
        if !available.is_empty() {
            lines.push(String::new());
        }
        lines.push(format!("  {}", style("auto-update on launch").bold()));
        for (index, subject) in setting_names.iter().enumerate() {
            let pointer = if available.len() + index == selection.cursor() {
                "❯"
            } else {
                " "
            };
            let name = format!("{:<width$}", subject, width = width);
            lines.push(format!(
                "  {pointer}   {name}  {}",
                policy_cell(selection.policies()[index])
            ));
        }
    }

    lines.push(String::new());
    let mut hints: Vec<String> = Vec::new();
    if !available.is_empty() {
        hints.push("space toggle".into());
    }
    if !setting_names.is_empty() {
        hints.push("←/→ setting".into());
    }
    if !available.is_empty() {
        hints.push("a all · n none".into());
        hints.push(format!("enter update {}", selection.count()));
    } else {
        hints.push("enter save".into());
    }
    hints.push("esc cancel".into());
    lines.push(format!("  {}", style(hints.join(" · ")).dim()));
    lines
}

/// `‹ on ›` — the arrows say "this cycles" without a legend.
fn policy_cell(policy: AutoUpdatePolicy) -> String {
    let label = match policy {
        AutoUpdatePolicy::On => style(policy.label()).green(),
        AutoUpdatePolicy::Notify => style(policy.label()).yellow(),
        AutoUpdatePolicy::Off => style(policy.label()).dim(),
    };
    format!("{} {label} {}", style("‹").dim(), style("›").dim())
}

#[cfg(test)]
mod tests {
    use super::*;
    use AutoUpdatePolicy::{Notify, Off, On};

    fn update(from: &str, to: &str) -> Available {
        Available {
            from: from.to_string(),
            to: to.to_string(),
            pinned: false,
        }
    }

    fn pinned_update(from: &str, to: &str) -> Available {
        Available {
            pinned: true,
            ..update(from, to)
        }
    }

    /// Update rows only, none pinned — the shape `ovm update <product>` uses.
    fn updates_only(len: usize) -> Selection {
        Selection::new(&vec![false; len], Vec::new())
    }

    #[test]
    fn everything_unpinned_starts_selected_so_the_common_answer_is_one_keypress() {
        let selection = updates_only(3);
        assert_eq!(selection.chosen(), vec![0, 1, 2]);
        assert_eq!(selection.count(), 3);
    }

    #[test]
    fn pinned_rows_start_unticked_so_enter_alone_never_moves_a_pin() {
        let selection = Selection::new(&[false, true, false], Vec::new());
        assert_eq!(selection.chosen(), vec![0, 2]);
    }

    #[test]
    fn space_deselects_the_row_under_the_cursor() {
        let mut selection = updates_only(3);
        selection.handle(&Key::ArrowDown);
        selection.handle(&Key::Char(' '));

        assert_eq!(selection.chosen(), vec![0, 2]);
    }

    #[test]
    fn space_toggles_back_on() {
        let mut selection = updates_only(2);
        selection.handle(&Key::Char(' '));
        selection.handle(&Key::Char(' '));

        assert_eq!(selection.chosen(), vec![0, 1]);
    }

    #[test]
    fn ticking_a_pinned_row_is_allowed_it_is_just_not_the_default() {
        let mut selection = Selection::new(&[true], Vec::new());
        assert!(selection.chosen().is_empty());
        selection.handle(&Key::Char(' '));
        assert_eq!(selection.chosen(), vec![0]);
    }

    #[test]
    fn a_and_n_select_and_clear_everything() {
        let mut selection = updates_only(3);
        assert_eq!(selection.handle(&Key::Char('n')), Step::Continue);
        assert!(selection.chosen().is_empty());
        selection.handle(&Key::Char('a'));
        assert_eq!(selection.chosen(), vec![0, 1, 2]);
    }

    #[test]
    fn the_cursor_wraps_across_both_sections() {
        let mut selection = Selection::new(&[false, false], vec![On]);
        selection.handle(&Key::ArrowUp);
        assert_eq!(
            selection.cursor(),
            2,
            "up from the top wraps to the last policy row"
        );
        selection.handle(&Key::ArrowDown);
        assert_eq!(
            selection.cursor(),
            0,
            "down from the bottom wraps to the top"
        );
    }

    #[test]
    fn vim_keys_move_too() {
        let mut selection = updates_only(3);
        selection.handle(&Key::Char('j'));
        assert_eq!(selection.cursor(), 1);
        selection.handle(&Key::Char('k'));
        assert_eq!(selection.cursor(), 0);
    }

    #[test]
    fn space_on_a_policy_row_cycles_the_policy() {
        let mut selection = Selection::new(&[], vec![Off]);
        selection.handle(&Key::Char(' '));
        assert_eq!(selection.policies(), &[On]);
        selection.handle(&Key::Char(' '));
        assert_eq!(selection.policies(), &[Notify]);
        selection.handle(&Key::Char(' '));
        assert_eq!(selection.policies(), &[Off], "the cycle closes");
    }

    #[test]
    fn arrows_cycle_a_policy_row_in_both_directions() {
        let mut selection = Selection::new(&[], vec![On]);
        selection.handle(&Key::ArrowRight);
        assert_eq!(selection.policies(), &[Notify]);
        selection.handle(&Key::ArrowLeft);
        selection.handle(&Key::ArrowLeft);
        assert_eq!(selection.policies(), &[Off]);
    }

    #[test]
    fn arrows_on_an_update_row_change_nothing() {
        let mut selection = Selection::new(&[false], vec![On]);
        selection.handle(&Key::ArrowRight);
        selection.handle(&Key::ArrowLeft);
        assert_eq!(selection.chosen(), vec![0]);
        assert_eq!(selection.policies(), &[On]);
    }

    #[test]
    fn select_all_and_none_leave_policies_alone() {
        // `a` means "all the updates", never "flip every launch setting".
        let mut selection = Selection::new(&[false, false], vec![Notify]);
        selection.handle(&Key::Char('a'));
        selection.handle(&Key::Char('n'));
        assert_eq!(selection.policies(), &[Notify]);
    }

    #[test]
    fn space_on_an_update_row_leaves_policies_alone() {
        let mut selection = Selection::new(&[false], vec![Off]);
        selection.handle(&Key::Char(' '));
        assert_eq!(selection.policies(), &[Off]);
        assert!(selection.chosen().is_empty());
    }

    #[test]
    fn enter_confirms_and_escape_cancels() {
        let mut selection = updates_only(2);
        assert_eq!(selection.handle(&Key::Enter), Step::Confirm);
        assert_eq!(selection.handle(&Key::Escape), Step::Cancel);
        assert_eq!(selection.handle(&Key::Char('q')), Step::Cancel);
    }

    #[test]
    fn unknown_keys_do_nothing_rather_than_confirming() {
        // A stray keypress must never be read as "yes, install".
        let mut selection = updates_only(2);
        assert_eq!(selection.handle(&Key::Char('z')), Step::Continue);
        assert_eq!(selection.handle(&Key::Tab), Step::Continue);
        assert_eq!(selection.chosen(), vec![0, 1]);
    }

    #[test]
    fn confirming_with_nothing_ticked_is_an_empty_choice_not_everything() {
        let mut selection = updates_only(2);
        selection.handle(&Key::Char('n'));
        assert_eq!(selection.handle(&Key::Enter), Step::Confirm);
        assert!(selection.chosen().is_empty());
    }

    #[test]
    fn the_frame_shows_every_update_and_the_live_count() {
        let claude = update("2.1.220", "2.1.221");
        let codex = update("rust-v0.145.0", "rust-v0.146.0");
        let available = vec![(Product::Claude, &claude), (Product::Codex, &codex)];
        let mut selection = Selection::new(&[false, false], Vec::new());
        selection.handle(&Key::Char(' ')); // untick the first

        let frame = render(&available, &[], &selection).join("\n");

        assert!(frame.contains("2 update(s) available"));
        assert!(frame.contains("2.1.220") && frame.contains("2.1.221"));
        assert!(frame.contains("rust-v0.146.0"));
        assert!(
            frame.contains("enter update 1"),
            "the count must track the ticks: {frame}"
        );
    }

    #[test]
    fn the_frame_labels_a_pinned_row() {
        let claude = pinned_update("2.1.220", "2.1.243");
        let available = vec![(Product::Claude, &claude)];
        let selection = Selection::new(&[true], Vec::new());

        let frame = render(&available, &[], &selection).join("\n");

        assert!(frame.contains("(pinned)"), "{frame}");
        assert!(
            frame.contains("enter update 0"),
            "a pinned row must not count as chosen by default: {frame}"
        );
    }

    #[test]
    fn the_frame_shows_the_settings_section_with_live_values() {
        let pi = update("0.80.10", "0.83.0");
        let available = vec![(Product::Pi, &pi)];
        let names = ["Claude Code", "OVM"];
        let mut selection = Selection::new(&[false], vec![On, Off]);
        // Move onto the last policy row and advance it: off → on.
        selection.handle(&Key::ArrowUp);
        selection.handle(&Key::ArrowRight);

        let frame = render(&available, &names, &selection).join("\n");

        assert!(frame.contains("auto-update on launch"), "{frame}");
        assert!(frame.contains("Claude Code") && frame.contains("OVM"));
        assert_eq!(selection.policies(), &[On, On]);
        assert!(frame.contains("←/→ setting"), "{frame}");
    }

    #[test]
    fn a_settings_only_frame_offers_save_instead_of_update_zero() {
        let selection = Selection::new(&[], vec![On, On, On, On]);
        let frame = render(&[], &["Claude Code", "Codex", "Pi", "OVM"], &selection).join("\n");

        assert!(!frame.contains("update(s) available"), "{frame}");
        assert!(frame.contains("enter save"), "{frame}");
        assert!(!frame.contains("a all"), "{frame}");
    }
}
