//! The small questions a picker asks without leaving its screen.

use console::{style, Key, Term};
use std::io;

/// An inline `y/N`. True only on `y` or `Y`; any other key is a no, so the
/// destructive branch is never one stray keypress away.
pub fn confirm_inline(term: &Term, prompt: &str) -> io::Result<bool> {
    term.write_line("")?;
    term.write_line(&format!(
        "  {} {} {}",
        style("?").yellow().bold(),
        prompt,
        style("[y/N]").dim()
    ))?;
    let key = term.read_key()?;
    let confirmed = matches!(key, Key::Char('y') | Key::Char('Y'));
    term.clear_last_lines(2)?;
    Ok(confirmed)
}

/// An inline free-text question, prefilled with `initial`. Returns the
/// trimmed answer, or `None` when it was left empty — which is how a person
/// says "never mind" to a prompt that has no Escape.
pub fn read_line_inline(term: &Term, prompt: &str, initial: &str) -> io::Result<Option<String>> {
    term.write_line("")?;
    term.write_str(&format!("  {} {} ", style("?").yellow().bold(), prompt))?;
    term.show_cursor()?;
    let answer = term.read_line_initial_text(initial);
    term.hide_cursor()?;
    let answer = answer?;
    term.clear_last_lines(2)?;
    let answer = answer.trim();
    Ok((!answer.is_empty()).then(|| answer.to_string()))
}

/// One keypress answers a `[Y/n]` / `[y/N]` prompt already printed by the
/// caller: `y`, `n`, Enter for the default, Escape for no. Any other key is
/// ignored rather than guessed at. The terminal is in raw mode for the read,
/// so the answer is echoed here with the newline that ends the prompt line.
///
/// `None` means a key could not be read — `console` reports a non-terminal
/// as `Key::Unknown` without blocking, so treating it as "some other key"
/// would spin forever on `2>file`. Callers fall back to a line read, which is
/// what every prompt did before keypresses, so a piped or scripted run keeps
/// working. Ctrl-C still quits: `read_key` restores the terminal and raises
/// SIGINT on the process.
///
/// This is the one rule for the hatch path: yes/no answers on the keypress,
/// only free text takes Enter. The tour asked on a keypress and the claudex
/// wizard it launches read a line, so a reader learned to press Enter after
/// `y` in one screen and had it leak into the next (2026-09-12).
pub fn confirm_key(term: &Term, default_yes: bool) -> io::Result<Option<bool>> {
    if !term.is_term() {
        return Ok(None);
    }
    loop {
        let answer = match term.read_key()? {
            Key::Enter => default_yes,
            Key::Char('y' | 'Y') => true,
            Key::Char('n' | 'N') | Key::Escape => false,
            Key::Unknown => return Ok(None),
            _ => continue,
        };
        eprintln!("{}", if answer { "y" } else { "n" });
        return Ok(Some(answer));
    }
}

/// The line-reading answer: `y`/`yes`, `n`/`no`, or empty for the default.
pub fn parse_confirm_line(input: &str, default_yes: bool) -> bool {
    let answer = input.trim().to_lowercase();
    if answer.is_empty() {
        return default_yes;
    }
    answer == "y" || answer == "yes"
}

/// Hold a message on screen until any key is pressed.
pub fn press_any_key(term: &Term, message: &str) -> io::Result<()> {
    term.write_line("")?;
    term.write_line(&format!("  {}", style(message).dim()))?;
    let _ = term.read_key();
    term.clear_last_lines(2)
}

#[cfg(test)]
mod tests {
    use super::parse_confirm_line;

    #[test]
    fn empty_takes_the_default() {
        assert!(parse_confirm_line("\n", true));
        assert!(!parse_confirm_line("", false));
    }

    #[test]
    fn yes_and_no_in_any_case() {
        for yes in ["y", "Y", "yes", "YES", " yes \n"] {
            assert!(parse_confirm_line(yes, false), "{yes:?}");
        }
        for no in ["n", "N", "no", "nope", "x"] {
            assert!(!parse_confirm_line(no, true), "{no:?}");
        }
    }
}
