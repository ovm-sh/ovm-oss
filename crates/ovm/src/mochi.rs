//! Mochi the Cat — little ASCII flourishes for user-facing output.
//! Mascot shared across mochiexists projects.
//!
//! Each face is padded so all three lines are the same column width,
//! which lets callers align trailing text consistently.

/// Default curious expression.
pub const DEFAULT: &str = "  /\\_/\\ \n ( o.o )\n  > ^ < ";

/// Happy — successful install / switch.
pub const HAPPY: &str = "  /\\_/\\ \n ( ^.^ )\n  > ^ < ";

/// Sad — error / failure.
pub const SAD: &str = "  /\\_/\\ \n ( u.u )\n  > ^ < ";

/// Working — busy doing something (auto-update, download).
pub const WORKING: &str = "  /\\_/\\ \n ( -.- )\n  > ^ < ";

/// Mochi's fur is ANSI magenta — the terminal theme's purple, the shade the
/// help banner always wore — in every mood; the mood color rides the message.
/// Deliberately the base color, not a fixed 256-color value, so the fur
/// matches the user's theme (a hard-coded 135 read as the wrong purple).
///
/// Targets stderr: `console` keys color detection to the destination stream,
/// and every stderr caller must use this variant so redirecting stderr to a
/// file strips the ANSI codes. Stdout callers use [`face_style_stdout`].
pub fn face_style(line: &str) -> console::StyledObject<&str> {
    console::style(line).for_stderr().magenta()
}

/// Same purple fur, color-detected against stdout (e.g. `ovm help`).
pub fn face_style_stdout(line: &str) -> console::StyledObject<&str> {
    console::style(line).magenta()
}

/// Print `face` to stderr with `message` aligned on the cat's middle line.
///
/// The faces are padded to a constant width (see module docs), so the message
/// lines up after the cat on every call. `message` is printed as-is, so callers
/// embed their own `console` styling; the art is always brand purple (see
/// [`face_style`]). A leading blank line gives the cat room to breathe.
pub fn say(face: &str, message: &str) {
    eprintln!();
    for (index, line) in face.lines().enumerate() {
        if index == 1 {
            eprintln!("{}  {message}", face_style(line));
        } else {
            eprintln!("{}", face_style(line));
        }
    }
}
