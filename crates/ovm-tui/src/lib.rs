//! The terminal primitives every OVM picker is built from.
//!
//! `ovm switch`, `ovm update`, and `ovm limits` each show a list, move a
//! cursor over it, and act on a key. They should feel like one program, so the
//! pieces that make a picker a picker live here, once: the alternate screen
//! ([`Screen`]), where keystrokes come from ([`Keys`]), the footer that names
//! the keys ([`footer`]), and the small prompts a picker asks in place
//! ([`confirm_inline`], [`read_line_inline`], [`press_any_key`]).
//!
//! Nothing here knows about products, versions, or accounts. A picker owns
//! its rows and its rules; this crate owns the frame around them.

mod keys;
mod prompt;
mod screen;
mod select;

pub use console::{style, Key, Term};
pub use keys::{Cue, Keys};
pub use prompt::{confirm_inline, press_any_key, read_line_inline};
pub use screen::{terminal_width, Screen};
pub use select::{select_one, Footer};

/// Fit a cell to a column: pad short values, cut long ones with an ellipsis.
/// Width-aware, so a wide glyph or an escape code does not skew the table.
pub fn fixed_width_cell(value: &str, width: usize) -> String {
    let truncated = console::truncate_str(value, width, "…");
    console::pad_str(&truncated, width, console::Alignment::Left, None).into_owned()
}

/// The one-line key legend under every picker: `↑↓ navigate · enter select`.
/// Each pair is (key, what it does); keys are bold, verbs are not.
pub fn footer(hints: &[(&str, &str)]) -> String {
    let parts: Vec<String> = hints
        .iter()
        .map(|(key, verb)| format!("{} {verb}", style(key).bold()))
        .collect();
    format!("  {}", parts.join(" · "))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &str) -> String {
        console::strip_ansi_codes(line).into_owned()
    }

    #[test]
    fn cells_pad_and_truncate_to_the_column() {
        assert_eq!(fixed_width_cell("ab", 4), "ab  ");
        assert_eq!(fixed_width_cell("abcdef", 4), "abc…");
        assert_eq!(fixed_width_cell("abcd", 4), "abcd");
    }

    #[test]
    fn the_footer_names_every_key_in_order() {
        let line = plain(&footer(&[
            ("↑↓", "navigate"),
            ("enter", "select"),
            ("esc", "quit"),
        ]));
        assert_eq!(line, "  ↑↓ navigate · enter select · esc quit");
    }
}
