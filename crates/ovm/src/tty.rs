//! Terminals reached through the `/dev/tty` alias.
//!
//! `/dev/tty` is not a terminal. It is the kernel's name for "whichever
//! terminal controls this process", and opening it yields a file that reads
//! and writes like the pty behind it — with one difference that matters here.
//! On macOS it cannot be watched with kqueue: the event loop is told nothing,
//! ever, so a runtime that waits for readability before reading waits forever.
//! Bun's loop does exactly that, and Claude Code is a Bun binary. Node is not
//! affected because libuv reopens a tty by name before polling it.
//!
//! The installer's `curl | sh` shape produces exactly this fd. Its stdin is the
//! pipe, so it re-attaches the terminal with `< /dev/tty` before handing over to
//! `ovm hatch` — and every child of the tour inherited the alias, the buddy
//! launch of Claude Code 2.1.96 included. Seen on 2026-09-03, in a live demo:
//! Claude drew its first screen and then ignored every key, with no way out
//! but closing the window (raw mode had swallowed ctrl-C too). Verified the
//! same day against 2.1.96 and 2.1.258 alike, and against bare `bun` 1.3.9:
//! stdin on the pty works, stdin on the alias does not.
//!
//! The repair is to swap fd 0 for the pty itself, found by name through
//! whichever of stdout or stderr is still the terminal. It runs once, at the
//! top of every invocation, so every launch path and every child inherits a
//! stdin that can be polled — not only the one the demo happened to take.

use std::ffi::CStr;
use std::fs::OpenOptions;
use std::os::unix::fs::OpenOptionsExt;
use std::os::unix::io::{AsRawFd, RawFd};
use std::path::{Path, PathBuf};

/// The kernel's controlling-terminal alias, on every Unix.
const ALIAS: &str = "/dev/tty";

/// The device name behind `fd`, if it is a terminal.
fn tty_name(fd: RawFd) -> Option<PathBuf> {
    // SAFETY: isatty only inspects the descriptor.
    if unsafe { libc::isatty(fd) } != 1 {
        return None;
    }
    let mut buf = [0 as libc::c_char; 256];
    // SAFETY: the buffer is exactly as long as ttyname_r is told it is.
    if unsafe { libc::ttyname_r(fd, buf.as_mut_ptr(), buf.len()) } != 0 {
        return None;
    }
    // SAFETY: ttyname_r NUL-terminates on success.
    let name = unsafe { CStr::from_ptr(buf.as_ptr()) };
    Some(PathBuf::from(name.to_string_lossy().into_owned()))
}

fn is_alias(path: &Path) -> bool {
    path == Path::new(ALIAS)
}

/// The terminal stdin should be replaced with, given what each standard
/// descriptor is named — or `None` when stdin is already fine or nothing
/// better is known.
///
/// Pure, so the decision is tested rather than the plumbing: only a stdin on
/// the alias is touched, and only when another descriptor names a real pty.
pub(crate) fn replacement_for(stdin: Option<&Path>, others: &[Option<PathBuf>]) -> Option<PathBuf> {
    if !stdin.is_some_and(is_alias) {
        return None;
    }
    others
        .iter()
        .flatten()
        .find(|path| !is_alias(path))
        .cloned()
}

/// Put the real pty on fd 0 when stdin is the `/dev/tty` alias.
///
/// Returns the path adopted, for the caller's diagnostics; `None` means stdin
/// was left as it was. Every failure is silent by design — a terminal that
/// cannot be reopened is a terminal that was working before this ran, and the
/// worst outcome of not swapping is the bug this repairs, not a new one.
pub fn adopt_real_terminal_stdin() -> Option<PathBuf> {
    let stdin = tty_name(libc::STDIN_FILENO);
    let others = [tty_name(libc::STDOUT_FILENO), tty_name(libc::STDERR_FILENO)];
    let path = replacement_for(stdin.as_deref(), &others)?;
    // Read-write, the way a shell hands a terminal to a command. O_NOCTTY so
    // that a process without a controlling terminal does not acquire one just
    // by opening a pty by name.
    let mut options = OpenOptions::new();
    options.read(true).write(true).custom_flags(libc::O_NOCTTY);
    let file = options.open(&path).or_else(|_| {
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_NOCTTY)
            .open(&path)
    });
    let file = file.ok()?;
    // SAFETY: dup2 onto a descriptor this process owns; `file` closes its own
    // descriptor on drop and fd 0 keeps the duplicate.
    if unsafe { libc::dup2(file.as_raw_fd(), libc::STDIN_FILENO) } < 0 {
        return None;
    }
    Some(path)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(name: &str) -> Option<PathBuf> {
        Some(PathBuf::from(name))
    }

    /// The case the demo hit: stdin on the alias, stdout on the pty.
    #[test]
    fn a_stdin_on_the_alias_is_replaced_by_the_pty_stdout_names() {
        let chosen = replacement_for(Some(Path::new(ALIAS)), &[path("/dev/ttys004"), None]);
        assert_eq!(chosen, path("/dev/ttys004"));
    }

    /// `curl | sh 2>&1 | tee log` leaves only stderr… or nothing. Take what
    /// there is, in order, and skip a descriptor that is itself the alias.
    #[test]
    fn the_first_real_pty_wins_and_aliases_are_skipped() {
        let chosen = replacement_for(Some(Path::new(ALIAS)), &[path(ALIAS), path("/dev/pts/3")]);
        assert_eq!(chosen, path("/dev/pts/3"));
        assert_eq!(
            replacement_for(Some(Path::new(ALIAS)), &[None, None]),
            None,
            "nothing better known: leave stdin alone",
        );
    }

    /// A stdin that is already the pty — or not a terminal at all — is never
    /// touched, whatever the other descriptors say.
    #[test]
    fn a_stdin_that_is_not_the_alias_is_left_alone() {
        assert_eq!(
            replacement_for(
                Some(Path::new("/dev/ttys004")),
                &[path("/dev/ttys004"), None]
            ),
            None
        );
        assert_eq!(replacement_for(None, &[path("/dev/ttys004"), None]), None);
    }

    /// The live half of the decision: a pty slave is named as itself, and an
    /// fd opened through the alias is named as the alias — the two facts
    /// `replacement_for` is built on. Skipped where the test process has no
    /// controlling terminal (CI), since the alias cannot be opened there.
    #[test]
    fn device_names_tell_a_pty_from_the_alias() {
        let mut leader: libc::c_int = -1;
        let mut follower: libc::c_int = -1;
        // SAFETY: openpty fills the two descriptors it is given; the optional
        // name, termios and winsize arguments are allowed to be null.
        let opened = unsafe {
            libc::openpty(
                &mut leader,
                &mut follower,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        assert_eq!(opened, 0, "openpty failed");
        let name = tty_name(follower).expect("a pty follower is a terminal");
        assert!(
            !is_alias(&name),
            "a pty is named as itself: {}",
            name.display()
        );
        // SAFETY: closing descriptors this test opened.
        unsafe {
            libc::close(leader);
            libc::close(follower);
        }

        if let Ok(alias) = OpenOptions::new().read(true).open(ALIAS) {
            let name = tty_name(alias.as_raw_fd()).expect("the alias is a terminal");
            assert!(
                is_alias(&name),
                "opened through the alias, named as the alias"
            );
        }
    }
}
