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

/// Hold a message on screen until any key is pressed.
pub fn press_any_key(term: &Term, message: &str) -> io::Result<()> {
    term.write_line("")?;
    term.write_line(&format!("  {}", style(message).dim()))?;
    let _ = term.read_key();
    term.clear_last_lines(2)
}
