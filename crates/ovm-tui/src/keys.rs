//! Where a picker's keystrokes come from.

use console::{style, Key, Term};
use std::collections::VecDeque;
use std::io;

/// One coached keypress: the key the reader is asked for, and what it does.
pub type Cue = (Key, &'static str);

/// Where a picker's keystrokes come from.
///
/// The tour teaches a gesture through the real picker: it names the one key
/// to press next, waits for the reader to press it, and lets the picker do
/// what that key always does. The picker still renders every frame it always
/// renders — only where the input comes from changes — which is what makes
/// the lesson honest rather than a mock-up of one.
///
/// An earlier cut had OVM press the keys itself, with the reader told to keep
/// their hands off. A gesture watched is not a gesture learned, and the
/// warning existed only because a press that arrived mid-script queued up
/// behind it. With the reader driving there is nothing to queue behind: a key
/// that is not the one asked for is dropped and named on the status line, so
/// the picker never wanders off the gesture and the reader always knows why
/// their press did nothing.
pub enum Keys {
    /// A person is driving. Read the real terminal.
    User,
    /// A person is driving, one named key at a time.
    Coached {
        /// The keys still to be pressed, in order.
        cues: VecDeque<Cue>,
        /// The last key that was not the one asked for, until the right one
        /// lands. Named on the status line so a dropped press reads as a
        /// nudge rather than a picker that stopped listening.
        stray: Option<Key>,
    },
}

impl Keys {
    pub fn coached(cues: impl IntoIterator<Item = Cue>) -> Self {
        Keys::Coached {
            cues: cues.into_iter().collect(),
            stray: None,
        }
    }

    /// The key the reader is being asked for, with its caption, if the lesson
    /// has one left.
    pub fn cue(&self) -> Option<&Cue> {
        match self {
            Keys::User => None,
            Keys::Coached { cues, .. } => cues.front(),
        }
    }

    /// The status line for a coached press, or nothing when the lesson is
    /// over and the picker is the ordinary picker again.
    pub fn status_line(&self) -> Option<String> {
        let Keys::Coached { cues, stray } = self else {
            return None;
        };
        let (key, caption) = cues.front()?;
        let asked = style(key_name(key)).bold();
        let nudge = match stray {
            Some(stray) => format!("not {} — ", style(key_name(stray)).bold()),
            None => String::new(),
        };
        Some(format!(
            "  {} {} · {nudge}press {asked} — {caption}",
            style("⌨").yellow().bold(),
            style("Your turn").bold(),
        ))
    }

    /// Pass a keypress through the lesson.
    ///
    /// The key asked for advances the lesson and goes to the picker unchanged.
    /// The keys that leave a picker (`Esc`, `q`) go through too: the reader
    /// can always walk out, and a lesson they left is not an error. Anything
    /// else is dropped — returned as [`Key::Unknown`], which every picker
    /// ignores — and remembered for the status line. Once the cues run out,
    /// every key goes through: the keyboard is simply the reader's.
    ///
    /// Separate from [`Keys::next`] so the rule is testable without a
    /// terminal.
    pub fn coach(&mut self, key: Key) -> Key {
        let Keys::Coached { cues, stray } = self else {
            return key;
        };
        let Some((asked, _)) = cues.front() else {
            return key;
        };
        if same_key(asked, &key) {
            cues.pop_front();
            *stray = None;
            return key;
        }
        if matches!(key, Key::Escape | Key::Char('q')) {
            return key;
        }
        *stray = Some(key);
        Key::Unknown
    }

    /// The next keystroke.
    pub fn next(&mut self, term: &Term) -> io::Result<Key> {
        let key = term.read_key()?;
        Ok(self.coach(key))
    }
}

/// A cue for `b` is met by `B` as well: the pickers accept either, and a
/// reader with caps lock on should not be told their `b` was not a `b`.
fn same_key(asked: &Key, pressed: &Key) -> bool {
    match (asked, pressed) {
        (Key::Char(a), Key::Char(p)) => a.eq_ignore_ascii_case(p),
        _ => asked == pressed,
    }
}

/// How a key is named on the status line: the way the footer names it.
fn key_name(key: &Key) -> String {
    match key {
        Key::Enter => "Enter".into(),
        Key::Escape => "Esc".into(),
        Key::Tab => "Tab".into(),
        Key::Backspace => "Backspace".into(),
        Key::Del => "Del".into(),
        Key::Home => "Home".into(),
        Key::End => "End".into(),
        Key::PageUp => "PageUp".into(),
        Key::PageDown => "PageDown".into(),
        Key::ArrowUp => "↑".into(),
        Key::ArrowDown => "↓".into(),
        Key::ArrowLeft => "←".into(),
        Key::ArrowRight => "→".into(),
        Key::Char(' ') => "Space".into(),
        Key::Char(c) => c.to_string(),
        _ => "that key".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lesson() -> Keys {
        Keys::coached([(Key::Char('b'), "filter"), (Key::Enter, "take the row")])
    }

    fn plain(keys: &Keys) -> String {
        console::strip_ansi_codes(&keys.status_line().expect("a status line")).into_owned()
    }

    #[test]
    fn the_key_asked_for_advances_the_lesson_and_reaches_the_picker() {
        let mut keys = lesson();
        assert_eq!(plain(&keys), "  ⌨ Your turn · press b — filter");
        assert_eq!(keys.coach(Key::Char('b')), Key::Char('b'));
        assert_eq!(plain(&keys), "  ⌨ Your turn · press Enter — take the row");
        assert_eq!(keys.coach(Key::Enter), Key::Enter);
        assert!(keys.cue().is_none());
        assert!(keys.status_line().is_none());
    }

    #[test]
    fn a_stray_key_is_dropped_and_named_until_the_right_one_lands() {
        let mut keys = lesson();
        assert_eq!(keys.coach(Key::ArrowUp), Key::Unknown);
        assert_eq!(plain(&keys), "  ⌨ Your turn · not ↑ — press b — filter");
        assert_eq!(keys.coach(Key::Char('x')), Key::Unknown);
        assert_eq!(plain(&keys), "  ⌨ Your turn · not x — press b — filter");
        assert_eq!(keys.coach(Key::Char('b')), Key::Char('b'));
        assert_eq!(plain(&keys), "  ⌨ Your turn · press Enter — take the row");
    }

    #[test]
    fn the_cue_is_met_in_either_case() {
        let mut keys = lesson();
        assert_eq!(keys.coach(Key::Char('B')), Key::Char('B'));
        assert_eq!(keys.cue().map(|(key, _)| key.clone()), Some(Key::Enter));
    }

    #[test]
    fn leaving_the_picker_is_always_allowed_and_does_not_advance_the_lesson() {
        let mut keys = lesson();
        assert_eq!(keys.coach(Key::Escape), Key::Escape);
        assert_eq!(keys.coach(Key::Char('q')), Key::Char('q'));
        assert_eq!(keys.cue().map(|(key, _)| key.clone()), Some(Key::Char('b')));
    }

    #[test]
    fn a_finished_lesson_lets_every_key_through() {
        let mut keys = Keys::coached([(Key::Enter, "go")]);
        keys.coach(Key::Enter);
        assert_eq!(keys.coach(Key::ArrowDown), Key::ArrowDown);
        assert_eq!(keys.coach(Key::Char('i')), Key::Char('i'));
    }

    #[test]
    fn a_person_driving_has_no_cue_and_no_status_line() {
        let mut keys = Keys::User;
        assert!(keys.cue().is_none());
        assert!(keys.status_line().is_none());
        assert_eq!(keys.coach(Key::Char('b')), Key::Char('b'));
    }
}
