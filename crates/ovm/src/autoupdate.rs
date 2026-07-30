//! Shared launch-time auto-update runtime.
//!
//! Products and OVM itself share the same three-state policy (`on | off |
//! notify`). This module holds the parts that are common to both:
//!   - [`decide_action`], the pure policy decision (unit-tested without a
//!     terminal), which the launch path drives for each subject;
//!   - the per-subject `notify` snooze cache, which dedups a given version's
//!     notice for three days but always re-announces a newer one;
//!   - [`prompt_notify`], the one-keypress install/snooze prompt with a short
//!     timeout so an unattended terminal never hangs.
//!
//! The self-update orchestration that consumes these lives in
//! [`crate::commands::self_autoupdate`]; product launch-time updates consume
//! them from [`crate::commands::launch`].

use crate::config::AutoUpdatePolicy;
use crate::update_cache::now_secs;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::path::{Path, PathBuf};

/// A snoozed `notify` version stays silent for three days (or until a newer
/// version appears, whichever comes first).
const SNOOZE_SECS: u64 = 3 * 24 * 60 * 60;

/// The notify prompt defaults to snooze after this long so an unattended
/// terminal reaches the launch instead of blocking on input forever.
const PROMPT_TIMEOUT_SECS: u64 = 5;

/// What a launch-time update check should do for one subject (a product or
/// OVM itself).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UpdateAction {
    /// No newer version, policy is `off`, or the notice is snoozed.
    Idle,
    /// Policy `on`: apply the update (products install now; self stages).
    Apply,
    /// Policy `notify` on an interactive terminal: prompt the user.
    Prompt,
    /// Policy `notify` without a TTY: print a single one-line notice.
    Notice,
}

/// Pure launch-time decision shared by product and self updates.
///
/// `newer_available` already folds in the dev-snapshot exemption and the
/// version comparison; `snoozed` is the per-version three-day dedup consulted
/// only for `notify`. Keeping this a pure function lets the TTY/prompt routing
/// be tested without a real terminal.
pub fn decide_action(
    policy: AutoUpdatePolicy,
    newer_available: bool,
    is_tty: bool,
    snoozed: bool,
) -> UpdateAction {
    if !newer_available {
        return UpdateAction::Idle;
    }
    match policy {
        AutoUpdatePolicy::Off => UpdateAction::Idle,
        AutoUpdatePolicy::On => UpdateAction::Apply,
        AutoUpdatePolicy::Notify => {
            if snoozed {
                UpdateAction::Idle
            } else if is_tty {
                UpdateAction::Prompt
            } else {
                UpdateAction::Notice
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SnoozeRecord {
    version: String,
    snoozed_at: u64,
}

fn snooze_path(base: &Path, subject: &str) -> PathBuf {
    base.join("cache")
        .join("notify")
        .join(format!("{subject}.json"))
}

/// Whether `version` for `subject` is inside its three-day snooze window. A
/// snooze recorded for a DIFFERENT version never suppresses a newer one.
pub fn is_snoozed(base: &Path, subject: &str, version: &str) -> bool {
    snooze_is_active(load_snooze(base, subject).as_ref(), version, now_secs())
}

fn snooze_is_active(record: Option<&SnoozeRecord>, version: &str, now: u64) -> bool {
    match record {
        Some(record) => {
            record.version == version && now.saturating_sub(record.snoozed_at) <= SNOOZE_SECS
        }
        None => false,
    }
}

fn load_snooze(base: &Path, subject: &str) -> Option<SnoozeRecord> {
    let raw = std::fs::read_to_string(snooze_path(base, subject)).ok()?;
    serde_json::from_str(&raw).ok()
}

/// Record that `version` for `subject` was snoozed now. Best-effort: a failure
/// to persist just means the notice repeats next launch, never a broken launch.
pub fn record_snooze(base: &Path, subject: &str, version: &str) {
    let path = snooze_path(base, subject);
    let Some(parent) = path.parent() else {
        return;
    };
    if std::fs::create_dir_all(parent).is_err() {
        return;
    }
    let record = SnoozeRecord {
        version: version.to_string(),
        snoozed_at: now_secs(),
    };
    let Ok(payload) = serde_json::to_string_pretty(&record) else {
        return;
    };
    // Atomic write so a concurrent reader sees the old or new record, never a
    // torn one.
    let tmp = path.with_extension("json.tmp");
    if std::fs::write(&tmp, payload).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

/// The user's answer to a notify prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NotifyChoice {
    Install,
    Snooze,
}

/// One-keypress prompt on stderr with a ~5s timeout that defaults to snooze so
/// an unattended terminal never hangs. `label` is the message stem, e.g.
/// `"OVM 0.0.4 available"`.
pub fn prompt_notify(label: &str) -> NotifyChoice {
    eprint!("{label} — [i]nstall now, [s]nooze: ");
    let _ = std::io::stderr().flush();
    match read_single_key_timeout(PROMPT_TIMEOUT_SECS) {
        Some('i') | Some('I') | Some('y') | Some('Y') => {
            eprintln!("i");
            NotifyChoice::Install
        }
        Some('\u{3}') => {
            eprintln!("cancelled");
            NotifyChoice::Snooze
        }
        other => {
            // Echo what resolved the prompt so the transcript isn't ambiguous.
            eprintln!("{}", other.map(|_| "s").unwrap_or("(timed out)"));
            NotifyChoice::Snooze
        }
    }
}

/// Read a single keypress from stdin, returning `None` on timeout or any setup
/// failure (so the caller falls back to snooze). Unix-only: puts the terminal
/// in raw mode just long enough to poll for and read one byte.
#[cfg(unix)]
struct RawModeGuard {
    fd: libc::c_int,
    original: libc::termios,
}

#[cfg(unix)]
impl RawModeGuard {
    fn enable(fd: libc::c_int) -> std::io::Result<Self> {
        // SAFETY: the caller supplies a live terminal descriptor. tcgetattr
        // validates it before we retain the original mode for restoration.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        let mut raw = original;
        // Disable ISIG as well so Ctrl-C arrives as ETX instead of terminating
        // the process before this guard can restore the terminal.
        raw.c_lflag &= !(libc::ICANON | libc::ECHO | libc::ISIG);
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 0;
        // SAFETY: `fd` passed tcgetattr and `raw` is derived from its mode.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { fd, original })
    }
}

#[cfg(unix)]
impl Drop for RawModeGuard {
    fn drop(&mut self) {
        // SAFETY: the guard never owns or closes `fd`; its caller keeps the
        // descriptor alive for at least the guard's lifetime.
        unsafe {
            libc::tcsetattr(self.fd, libc::TCSANOW, &self.original);
        }
    }
}

#[cfg(unix)]
fn read_single_key_timeout(timeout_secs: u64) -> Option<char> {
    use std::os::unix::io::AsRawFd;

    let stdin = std::io::stdin();
    let fd = stdin.as_raw_fd();

    let _raw_mode = RawModeGuard::enable(fd).ok()?;
    poll_single_key(fd, timeout_secs)
}

#[cfg(unix)]
fn poll_single_key(fd: libc::c_int, timeout_secs: u64) -> Option<char> {
    let mut poll_fd = libc::pollfd {
        fd,
        events: libc::POLLIN,
        revents: 0,
    };
    let timeout_ms = (timeout_secs.saturating_mul(1000)).min(i32::MAX as u64) as libc::c_int;
    let ready = unsafe { libc::poll(&mut poll_fd, 1, timeout_ms) };

    let mut key = None;
    if ready > 0 && (poll_fd.revents & libc::POLLIN) != 0 {
        let mut buffer = [0u8; 1];
        let read = unsafe { libc::read(fd, buffer.as_mut_ptr() as *mut libc::c_void, 1) };
        if read == 1 {
            key = Some(buffer[0] as char);
        }
    }

    key
}

#[cfg(not(unix))]
fn read_single_key_timeout(_timeout_secs: u64) -> Option<char> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn off_and_missing_updates_stay_idle() {
        assert_eq!(
            decide_action(AutoUpdatePolicy::On, false, true, false),
            UpdateAction::Idle
        );
        assert_eq!(
            decide_action(AutoUpdatePolicy::Off, true, true, false),
            UpdateAction::Idle
        );
    }

    #[test]
    fn on_applies_when_newer_is_available() {
        assert_eq!(
            decide_action(AutoUpdatePolicy::On, true, false, false),
            UpdateAction::Apply
        );
    }

    #[test]
    fn notify_routes_on_tty_and_snooze() {
        // Interactive terminal, not snoozed -> prompt.
        assert_eq!(
            decide_action(AutoUpdatePolicy::Notify, true, true, false),
            UpdateAction::Prompt
        );
        // No TTY -> one-line notice, never a prompt.
        assert_eq!(
            decide_action(AutoUpdatePolicy::Notify, true, false, false),
            UpdateAction::Notice
        );
        // Snoozed -> silent regardless of TTY.
        assert_eq!(
            decide_action(AutoUpdatePolicy::Notify, true, true, true),
            UpdateAction::Idle
        );
        assert_eq!(
            decide_action(AutoUpdatePolicy::Notify, true, false, true),
            UpdateAction::Idle
        );
    }

    #[test]
    fn snooze_dedups_same_version_but_not_a_newer_one() {
        let dir = tempdir().unwrap();
        let base = dir.path();
        assert!(!is_snoozed(base, "self", "0.0.4"));

        record_snooze(base, "self", "0.0.4");
        assert!(is_snoozed(base, "self", "0.0.4"));
        // A newer version is never suppressed by an older snooze.
        assert!(!is_snoozed(base, "self", "0.0.5"));
        // Snoozes are per subject.
        assert!(!is_snoozed(base, "codex", "0.0.4"));
    }

    #[test]
    fn snooze_expires_after_the_window() {
        let now = 1_000_000_000;
        let fresh = SnoozeRecord {
            version: "0.0.4".into(),
            snoozed_at: now,
        };
        assert!(snooze_is_active(Some(&fresh), "0.0.4", now + SNOOZE_SECS));
        assert!(!snooze_is_active(
            Some(&fresh),
            "0.0.4",
            now + SNOOZE_SECS + 1
        ));
        assert!(!snooze_is_active(None, "0.0.4", now));
    }

    #[test]
    #[cfg(unix)]
    fn raw_mode_guard_restores_terminal_on_drop() {
        let mut master = 0;
        let mut slave = 0;
        // SAFETY: openpty initializes both descriptors; null termios/winsize
        // pointers request the platform defaults for the new pseudo-terminal.
        assert_eq!(
            unsafe {
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            },
            0
        );

        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: `slave` is a valid PTY descriptor until cleanup below.
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut original) }, 0);
        {
            let _guard = RawModeGuard::enable(slave).expect("enable raw mode");
            let mut raw: libc::termios = unsafe { std::mem::zeroed() };
            // SAFETY: same live PTY descriptor.
            assert_eq!(unsafe { libc::tcgetattr(slave, &mut raw) }, 0);
            assert_eq!(raw.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG), 0);

            let etx = [3_u8];
            // SAFETY: `master` is the live peer of `slave`, and `etx` is a
            // one-byte initialized buffer.
            assert_eq!(
                unsafe { libc::write(master, etx.as_ptr() as *const libc::c_void, etx.len(),) },
                1
            );
            assert_eq!(poll_single_key(slave, 1), Some('\u{3}'));
        }

        let mut restored: libc::termios = unsafe { std::mem::zeroed() };
        // SAFETY: same live PTY descriptor.
        assert_eq!(unsafe { libc::tcgetattr(slave, &mut restored) }, 0);
        assert_eq!(
            restored.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG),
            original.c_lflag & (libc::ICANON | libc::ECHO | libc::ISIG)
        );
        assert_eq!(restored.c_cc, original.c_cc);

        // SAFETY: both descriptors were returned by openpty and are closed once.
        unsafe {
            libc::close(master);
            libc::close(slave);
        }
    }
}
