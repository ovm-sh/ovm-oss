//! One left edge, across a process boundary.
//!
//! `ovm hatch` centres its prose on a shared margin and runs this wizard as a
//! CHILD PROCESS, so the margin — a `OnceLock` inside the tour — could not
//! reach it. Every line here printed flush-left inside a centred tour, and the
//! handoff read as a different program taking over. Which, literally, it is;
//! but that is an implementation detail the reader should never have to see.
//!
//! The tour now hands its margin down through [`INDENT_ENV`], and everything
//! printed through [`say!`] sits on it. Run `ovm claudex setup` directly and
//! the variable is unset, the margin is empty, and the output is exactly what
//! it always was.
//!
//! Indenting alone would have made things worse: a 75-column line inside a
//! 22-column margin overflows a 106-column terminal that fitted it before. So
//! the fold comes with it — the same algorithm the tour uses, ported rather
//! than shared because `ovm` is a binary crate with no library to depend on.
//! Keep the two in step; the tour's copy carries the fuller commentary.

use std::sync::OnceLock;

/// The margin `ovm hatch` centres its prose on, handed down to this process.
pub const INDENT_ENV: &str = "OVM_OUTPUT_INDENT";

/// Suppresses the parts of the intro the tour has already delivered. Set as a
/// flag by the caller rather than inferred from [`INDENT_ENV`]: layout and
/// content are different decisions and conflating them would mean a future
/// caller could not have one without the other.
pub const BRIEF_ENV: &str = "OVM_CLAUDEX_BRIEF";

struct Layout {
    indent: String,
    usable: usize,
}

fn layout() -> &'static Layout {
    static LAYOUT: OnceLock<Layout> = OnceLock::new();
    LAYOUT.get_or_init(|| {
        let indent = std::env::var(INDENT_ENV).unwrap_or_default();
        let width = console::Term::stderr()
            .size_checked()
            .map_or(80, |(_, columns)| columns as usize);
        // A terminal narrower than the margin is not worth laying out for, but
        // it must not produce a zero-width budget that puts every word on its
        // own line.
        // One column of headroom, not zero. A line that fills the terminal
        // exactly leaves the cursor past the last column and the emulator
        // wraps it — which is how an 85-column line inside a 22-column margin
        // came apart on a 107-column recording whose arithmetic said it fitted.
        let usable = width
            .saturating_sub(console::measure_text_width(&indent) + 1)
            .max(20);
        Layout { indent, usable }
    })
}

pub fn indent() -> &'static str {
    &layout().indent
}

/// Whether the caller has already introduced claudex to this reader.
pub fn brief() -> bool {
    std::env::var_os(BRIEF_ENV).is_some_and(|value| !value.is_empty())
}

/// Print one line on the margin, folded to what is left of the terminal.
pub fn say_line(text: &str) {
    for line in fold(text, layout().usable) {
        eprintln!("{}{line}", indent());
    }
}

/// Print a prompt on the margin, leaving the cursor after the last line so the
/// answer is typed where the reader is looking.
pub fn ask_line(text: &str) {
    let lines = fold(text, layout().usable);
    let (last, rest) = lines.split_last().expect("fold always yields a line");
    for line in rest {
        eprintln!("{}{line}", indent());
    }
    eprint!("{}{last}", indent());
}

/// Fold `text` to `width` display columns, hanging the continuation under the
/// line's own leading indent. Ported from the tour's `fold`.
fn fold(text: &str, width: usize) -> Vec<String> {
    if console::measure_text_width(text) <= width {
        return vec![text.to_owned()];
    }
    let body = text.trim_start_matches(' ');
    let indent = &text[..text.len() - body.len()];
    let inner = width.saturating_sub(console::measure_text_width(indent));
    let mut lines: Vec<String> = Vec::new();
    let mut current = String::new();
    for word in body.split(' ').filter(|word| !word.is_empty()) {
        for piece in split_word(word, inner) {
            if current.is_empty() {
                current = format!("{indent}{piece}");
            } else if console::measure_text_width(&current)
                + 1
                + console::measure_text_width(&piece)
                > width
            {
                lines.push(std::mem::take(&mut current));
                current = format!("{indent}{piece}");
            } else {
                current.push(' ');
                current.push_str(&piece);
            }
        }
    }
    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(text.to_owned());
    }
    lines
}

/// Break a word that cannot fit the budget even on a line of its own, stepping
/// over escape sequences whole so a break can never land inside one. Ported
/// from the tour's `split_word`.
fn split_word(word: &str, width: usize) -> Vec<String> {
    if width == 0 || console::measure_text_width(word) <= width {
        return vec![word.to_owned()];
    }
    let mut pieces: Vec<String> = Vec::new();
    let mut piece = String::new();
    let mut painted = 0usize;
    let mut characters = word.chars().peekable();
    while let Some(character) = characters.next() {
        if character == '\u{1b}' {
            piece.push(character);
            match characters.peek() {
                Some('[') => {
                    for escape in characters.by_ref() {
                        piece.push(escape);
                        if escape.is_ascii_alphabetic() {
                            break;
                        }
                    }
                }
                Some(']') => {
                    let mut after_escape = false;
                    for escape in characters.by_ref() {
                        piece.push(escape);
                        if escape == '\u{7}' || (after_escape && escape == '\\') {
                            break;
                        }
                        after_escape = escape == '\u{1b}';
                    }
                }
                _ => {
                    if let Some(escape) = characters.next() {
                        piece.push(escape);
                    }
                }
            }
            continue;
        }
        let character_width = console::measure_text_width(character.encode_utf8(&mut [0; 4]));
        if painted > 0 && painted + character_width > width {
            pieces.push(std::mem::take(&mut piece));
            painted = 0;
        }
        piece.push(character);
        painted += character_width;
    }
    if !piece.is_empty() {
        pieces.push(piece);
    }
    pieces
}

/// `eprintln!` on the caller's margin, folded.
macro_rules! say {
    () => { eprintln!() };
    ($($arg:tt)*) => { $crate::output::say_line(&format!($($arg)*)) };
}

/// `eprint!` on the caller's margin — for prompts that read on the same line.
macro_rules! ask {
    ($($arg:tt)*) => { $crate::output::ask_line(&format!($($arg)*)) };
}

pub(crate) use {ask, say};

#[cfg(test)]
mod tests {
    use super::*;

    /// The exact line, at the exact width, that came apart on camera: 85
    /// columns inside the tour's 22-column margin on a 107-column recording.
    /// The arithmetic said it fitted — the terminal wrapped it anyway, because
    /// a line ending on the last column leaves the cursor past it.
    #[test]
    fn a_line_that_ends_on_the_last_column_is_still_folded() {
        let line =
            "  ✓ Isolated Claude home seeded → ~/.ovm/claudex/claude (your ~/.claude is untouched)";
        assert_eq!(console::measure_text_width(line), 85);
        // width 107, indent 22 -> the old budget was exactly 85 and let it through.
        let folded = fold(line, 107 - 22 - 1);
        assert!(folded.len() > 1, "expected a fold, got {folded:?}");
        for piece in &folded {
            assert!(
                console::measure_text_width(piece) + 22 < 107,
                "still reaches the edge: {piece:?}"
            );
        }
    }

    #[test]
    fn a_line_that_fits_is_untouched() {
        let line = "  2. Configure the CLIProxyAPI sidecar (localhost-only, random key)";
        assert_eq!(fold(line, 72), vec![line.to_string()]);
    }

    #[test]
    fn a_folded_line_hangs_under_its_own_indent() {
        let folded = fold("    1. Install Claude Code if it isn't already", 24);
        assert!(folded.len() > 1, "expected a fold, got {folded:?}");
        for line in &folded[1..] {
            assert!(
                line.starts_with("    "),
                "continuation lost the indent: {line:?}"
            );
        }
    }

    /// Literal escapes rather than `console::style`: colour is stripped when
    /// stderr is not a terminal, so a styled string under `cargo test` can
    /// carry no escapes at all and the case would prove nothing.
    #[test]
    fn width_is_measured_in_columns_not_bytes() {
        let styled = "\u{1b}[32minstalled\u{1b}[0m";
        assert!(styled.len() > "installed".len(), "expected escapes");
        assert_eq!(fold(styled, 9), vec![styled.to_string()]);
    }

    /// The URL in the intro is one unbreakable word, and the reader's own
    /// statusline path can be longer still.
    #[test]
    fn an_unbreakable_word_is_split_rather_than_left_to_overflow() {
        let url = "https://example.com/a/very/long/path/that/cannot/fit";
        for piece in split_word(url, 20) {
            assert!(console::measure_text_width(&piece) <= 20, "{piece:?}");
        }
    }

    #[test]
    fn splitting_never_breaks_inside_an_escape_sequence() {
        let styled = "\u{1b}[36mabcdefghijklmnop\u{1b}[0m";
        let pieces = split_word(styled, 4);
        assert!(pieces.len() > 1, "expected a split, got {pieces:?}");
        assert_eq!(pieces.concat(), styled);
        for piece in &pieces {
            assert!(console::measure_text_width(piece) <= 4, "{piece:?}");
        }
    }
}
