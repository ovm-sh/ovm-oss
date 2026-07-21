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

/// Print `face` to stderr with `message` aligned on the cat's middle line.
///
/// The faces are padded to a constant width (see module docs), so the message
/// lines up after the cat on every call. `message` is printed as-is, so callers
/// embed their own `console` styling; the art stays in the terminal's default
/// color. A leading blank line gives the cat room to breathe.
pub fn say(face: &str, message: &str) {
    eprintln!();
    for (index, line) in face.lines().enumerate() {
        if index == 1 {
            eprintln!("{line}  {message}");
        } else {
            eprintln!("{line}");
        }
    }
}
