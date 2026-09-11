//! The frame a picker draws into.

use console::Term;
use std::io::{self, Write};

const DEFAULT_TERMINAL_WIDTH: usize = 80;

/// A picker's screen: the alternate buffer on a real terminal (so the picker
/// leaves no trace when it exits), plain line clearing anywhere else (so a
/// transcript or a pty test still reads sensibly). The cursor is hidden for
/// the duration and restored on exit — including the exit a panic takes.
pub struct Screen<'a> {
    term: &'a Term,
    alternate: bool,
}

impl<'a> Screen<'a> {
    pub fn enter(term: &'a Term) -> io::Result<Self> {
        let alternate = term.is_term();
        if alternate {
            write_terminal_escape("\x1b[?1049h\x1b[2J\x1b[H")?;
        }
        term.hide_cursor()?;
        Ok(Self { term, alternate })
    }

    /// Wipe the last frame so the next one can be drawn in its place.
    pub fn clear_frame(&self, last_line_count: usize) -> io::Result<()> {
        if self.alternate {
            write_terminal_escape("\x1b[H\x1b[2J")
        } else if last_line_count > 0 {
            self.term.clear_last_lines(last_line_count)
        } else {
            Ok(())
        }
    }

    /// Draw one frame: every line, in order.
    pub fn draw(&self, lines: &[String]) -> io::Result<()> {
        for line in lines {
            self.term.write_line(line)?;
        }
        Ok(())
    }

    /// Leave the screen the way it was found. Safe to call more than once;
    /// `Drop` calls it too, so an early `?` still restores the terminal.
    pub fn finish(&mut self) -> io::Result<()> {
        self.term.show_cursor()?;
        if self.alternate {
            write_terminal_escape("\x1b[?1049l")?;
            self.alternate = false;
        }
        Ok(())
    }
}

impl Drop for Screen<'_> {
    fn drop(&mut self) {
        let _ = self.term.show_cursor();
        if self.alternate {
            let _ = write_terminal_escape("\x1b[?1049l");
        }
    }
}

fn write_terminal_escape(sequence: &str) -> io::Result<()> {
    let mut stderr = io::stderr();
    stderr.write_all(sequence.as_bytes())?;
    stderr.flush()
}

/// The terminal's column count, or a sensible default when there is no
/// terminal to ask.
pub fn terminal_width(term: &Term) -> usize {
    term.size_checked()
        .map(|(_, cols)| cols as usize)
        .filter(|&cols| cols > 0)
        .unwrap_or(DEFAULT_TERMINAL_WIDTH)
}
