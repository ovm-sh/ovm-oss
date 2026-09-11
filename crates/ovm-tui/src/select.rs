//! The simplest picker: a titled list, a cursor, Enter or Escape.

use crate::{footer, Keys, Screen};
use console::{style, Key, Term};
use std::io;

/// What the legend under a list says. The navigation and confirm keys are
/// always there; the closing key's verb depends on whether there is a screen
/// to go back to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Footer {
    /// `esc quit`
    Quit,
    /// `esc back`
    Back,
}

/// Show `items` under `title` and return the index chosen, or `None` on
/// Escape. Items arrive rendered; this decides only where the cursor is.
pub fn select_one(
    term: &Term,
    title: &str,
    items: &[String],
    keys: &mut Keys,
    closing: Footer,
) -> io::Result<Option<usize>> {
    if items.is_empty() {
        return Ok(None);
    }
    let mut screen = Screen::enter(term)?;
    let mut cursor = 0usize;
    loop {
        let lines = render(title, items, cursor, keys.status_line(), closing);
        screen.draw(&lines)?;
        let key = keys.next(term)?;
        screen.clear_frame(lines.len())?;
        match key {
            Key::ArrowUp | Key::Char('k') => cursor = cursor.saturating_sub(1),
            Key::ArrowDown | Key::Char('j') if cursor + 1 < items.len() => cursor += 1,
            Key::Enter => {
                screen.finish()?;
                return Ok(Some(cursor));
            }
            Key::Escape | Key::Char('q') => {
                screen.finish()?;
                return Ok(None);
            }
            _ => {}
        }
    }
}

/// Pure, so the frame is testable without a terminal.
pub(crate) fn render(
    title: &str,
    items: &[String],
    cursor: usize,
    status: Option<String>,
    closing: Footer,
) -> Vec<String> {
    let mut lines = vec![
        String::new(),
        format!("  {}", style(title).bold()),
        String::new(),
    ];
    for (index, item) in items.iter().enumerate() {
        if index == cursor {
            lines.push(format!("{} {item}", style("›").cyan().bold()));
        } else {
            lines.push(format!("  {item}"));
        }
    }
    lines.push(String::new());
    if let Some(status) = status {
        lines.push(status);
    }
    let closing = match closing {
        Footer::Quit => "quit",
        Footer::Back => "back",
    };
    lines.push(footer(&[
        ("↑↓", "navigate"),
        ("enter", "select"),
        ("esc", closing),
    ]));
    lines
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[String]) -> Vec<String> {
        lines
            .iter()
            .map(|line| console::strip_ansi_codes(line).into_owned())
            .collect()
    }

    #[test]
    fn the_cursor_marks_one_row_and_the_footer_names_the_way_out() {
        let items = vec!["  claude".to_string(), "  codex".to_string()];
        let lines = plain(&render("Which product?", &items, 1, None, Footer::Back));
        assert_eq!(lines[1], "  Which product?");
        assert_eq!(lines[3], "    claude");
        assert_eq!(lines[4], "›   codex");
        assert_eq!(
            lines.last().unwrap(),
            "  ↑↓ navigate · enter select · esc back"
        );
    }

    #[test]
    fn a_scripted_caption_sits_above_the_footer() {
        let items = vec!["a".to_string()];
        let lines = plain(&render(
            "t",
            &items,
            0,
            Some("  ▶ driving".into()),
            Footer::Quit,
        ));
        let footer_index = lines.len() - 1;
        assert_eq!(lines[footer_index - 1], "  ▶ driving");
    }
}
