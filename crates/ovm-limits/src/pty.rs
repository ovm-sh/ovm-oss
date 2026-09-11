//! A minimal pseudo-terminal: enough to run an interactive TUI, type a line
//! into it, and let its output fall on the floor.
//!
//! No parsing happens here or anywhere else — the Claude poll reads the
//! statusline capture file, never the screen. The pty exists only because
//! Claude Code exposes usage windows to its statusline script exclusively in
//! interactive mode (`-p` runs no statusline; measured on 2.1.261).

use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

pub struct PtyChild {
    master: OwnedFd,
    pub child: Child,
    hung_up: bool,
    /// The tail of what the terminal has shown, for explaining a failure.
    ///
    /// A poll that times out used to throw the screen away and report one
    /// fixed sentence about not being signed in. On 2026-09-08 that sentence
    /// covered two unrelated causes on the same night — an inline auto-update
    /// download, and something else entirely — and neither could be told from
    /// the other afterwards, because the only evidence had been discarded.
    recent: Vec<u8>,
}

/// How much of the terminal tail to keep. Enough for the last screens, small
/// enough that a chatty session cannot grow the poll's memory without bound.
const RECENT_CAP: usize = 16 * 1024;

/// The command is taken by value on purpose: `Command` keeps every `Stdio`
/// it was given until it is dropped, and those are the parent's copies of the
/// slave. A parent that still holds the slave is a terminal that never hangs
/// up — and on macOS a session leader cannot finish exiting while another
/// process holds its controlling terminal open. With the caller keeping the
/// `Command` alive across the poll, `finish()` sat in `wait4` on a child stuck
/// in exit, for as long as anyone let it (42 minutes, 2026-09-08). Dropping the
/// command right after the fork closes the parent's slaves, so the child's
/// exit is a hangup on the master and `wait` returns.
pub fn spawn(mut command: Command, cols: u16, rows: u16) -> io::Result<PtyChild> {
    let mut master: libc::c_int = -1;
    let mut slave: libc::c_int = -1;
    let mut size = libc::winsize {
        ws_row: rows,
        ws_col: cols,
        ws_xpixel: 0,
        ws_ypixel: 0,
    };
    // Linux's libc declares the winsize argument `*const`, macOS's `*mut`;
    // the inferred cast lands on whichever this platform wants.
    let size_ptr = std::ptr::addr_of_mut!(size) as _;
    // SAFETY: openpty writes two descriptors through the out-pointers; the
    // winsize pointer is valid for the duration of the call.
    let rc = unsafe {
        libc::openpty(
            &mut master,
            &mut slave,
            std::ptr::null_mut(),
            std::ptr::null_mut(),
            size_ptr,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: both descriptors were just returned by openpty and nothing else
    // owns them.
    let master = unsafe { OwnedFd::from_raw_fd(master) };
    let slave = unsafe { OwnedFd::from_raw_fd(slave) };
    // openpty hands back inheritable descriptors (measured on macOS). Without
    // close-on-exec the child would hold its own copy of the master, and the
    // terminal could never hang up on it.
    // The slave too: the child gets its copies on 0, 1 and 2 from `Command`,
    // and must not inherit a fourth at whatever number this one happens to be.
    // SAFETY: fcntl on descriptors this function owns.
    for fd in [master.as_raw_fd(), slave.as_raw_fd()] {
        if unsafe { libc::fcntl(fd, libc::F_SETFD, libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    let stdout = slave.try_clone()?;
    let stderr = slave.try_clone()?;
    command
        .stdin(Stdio::from(slave))
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr));
    // SAFETY: runs in the forked child before exec; setsid and ioctl are
    // async-signal-safe and touch only the child's own session and fd 0.
    unsafe {
        command.pre_exec(|| {
            if libc::setsid() < 0 {
                return Err(io::Error::last_os_error());
            }
            if libc::ioctl(0, libc::TIOCSCTTY as _, 0) < 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(())
        });
    }
    let child = command.spawn()?;
    // Closes the parent's three slave descriptors — see the doc comment.
    drop(command);
    Ok(PtyChild {
        master,
        child,
        hung_up: false,
        recent: Vec::new(),
    })
}

impl PtyChild {
    /// Consume and discard terminal output for `timeout`. Returns early only
    /// if the terminal hangs up (the child exited), in which case the rest of
    /// the wait is slept so callers can use this as their pacing.
    pub fn drain(&mut self, timeout: Duration) {
        let deadline = Instant::now() + timeout;
        let mut buffer = [0u8; 8192];
        loop {
            let now = Instant::now();
            if now >= deadline {
                return;
            }
            let remaining = deadline - now;
            if self.hung_up {
                std::thread::sleep(remaining);
                return;
            }
            let mut poll = libc::pollfd {
                fd: self.master.as_raw_fd(),
                events: libc::POLLIN,
                revents: 0,
            };
            let millis = remaining.as_millis().min(i32::MAX as u128) as libc::c_int;
            // SAFETY: one valid pollfd for the given count.
            let ready = unsafe { libc::poll(&mut poll, 1, millis) };
            if ready == 0 {
                return;
            }
            if ready < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                self.hung_up = true;
                continue;
            }
            // SAFETY: reading into a buffer of the stated length.
            let read = unsafe {
                libc::read(
                    self.master.as_raw_fd(),
                    buffer.as_mut_ptr() as *mut libc::c_void,
                    buffer.len(),
                )
            };
            if read <= 0 {
                // EIO after the child closed its side is the normal hangup on
                // both macOS and Linux.
                self.hung_up = true;
            } else {
                self.remember(&buffer[..read as usize]);
            }
        }
    }

    pub fn hung_up(&self) -> bool {
        self.hung_up
    }

    /// Keep the tail of the terminal, dropping the oldest bytes past the cap.
    fn remember(&mut self, chunk: &[u8]) {
        if chunk.len() >= RECENT_CAP {
            self.recent = chunk[chunk.len() - RECENT_CAP..].to_vec();
            return;
        }
        self.recent.extend_from_slice(chunk);
        if self.recent.len() > RECENT_CAP {
            let excess = self.recent.len() - RECENT_CAP;
            self.recent.drain(..excess);
        }
    }

    /// What the terminal has shown lately, as text a human can read: escape
    /// sequences removed, blank runs collapsed, one line per line.
    ///
    /// This is for explaining a failure, not for parsing. Nothing decides
    /// anything from it — a poll's verdict comes from the statusline payload,
    /// never from scraping the screen.
    pub fn recent_text(&self) -> String {
        let raw = String::from_utf8_lossy(&self.recent);
        let mut out = String::with_capacity(raw.len());
        let mut chars = raw.chars();
        while let Some(ch) = chars.next() {
            match ch {
                // CSI / OSC and friends: skip to the terminator rather than
                // print the control bytes at whoever reads the log.
                '\u{1b}' => {
                    for next in chars.by_ref() {
                        if next.is_ascii_alphabetic() || next == '\u{7}' || next == '\\' {
                            break;
                        }
                    }
                }
                '\r' => out.push('\n'),
                c if c.is_control() && c != '\n' && c != '\t' => {}
                c => out.push(c),
            }
        }
        let mut lines: Vec<&str> = out.lines().map(str::trim_end).collect();
        lines.retain(|line| !line.trim().is_empty());
        lines.dedup();
        lines.join("\n")
    }

    /// Type bytes into the terminal as if from the keyboard.
    pub fn write_all(&mut self, mut bytes: &[u8]) -> io::Result<()> {
        while !bytes.is_empty() {
            // SAFETY: writing from a valid slice of the stated length.
            let written = unsafe {
                libc::write(
                    self.master.as_raw_fd(),
                    bytes.as_ptr() as *const libc::c_void,
                    bytes.len(),
                )
            };
            if written < 0 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::Interrupted {
                    continue;
                }
                return Err(error);
            }
            bytes = &bytes[written as usize..];
        }
        Ok(())
    }

    /// Give the child `grace` to exit on its own, then kill it — and everything
    /// it started. The child is its own session leader (`setsid` above), so its
    /// process group is exactly the tree under it: when `ovm claude` waits on
    /// the real `claude`, a plain kill of the launcher would orphan the TUI.
    ///
    /// Every wait in here keeps reading the master, and none of them is
    /// unbounded. A process cannot finish closing its terminal while the
    /// terminal still holds output nobody has read — the close waits for the
    /// master to take it — and SIGKILL does not help, because by then the
    /// process is already in exit. A parent that blocked in `wait` at that
    /// point sat there for 42 minutes with its child stuck in exit
    /// (2026-09-08). So: read, then check, and give up with an error rather
    /// than hang.
    pub fn finish(&mut self, grace: Duration) -> io::Result<()> {
        if self.reap(grace)? {
            return Ok(());
        }
        self.kill_group(libc::SIGTERM);
        if self.reap(Duration::from_secs(2))? {
            return Ok(());
        }
        self.kill_group(libc::SIGKILL);
        if self.reap(Duration::from_secs(5))? {
            return Ok(());
        }
        Err(io::Error::other(
            "the session did not exit after SIGKILL — it may be stuck closing its terminal",
        ))
    }

    /// Wait up to `timeout` for the child to exit, reading its terminal the
    /// whole time so its exit is never blocked on us. True once it is gone.
    fn reap(&mut self, timeout: Duration) -> io::Result<bool> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.child.try_wait()?.is_some() {
                return Ok(true);
            }
            if Instant::now() >= deadline {
                return Ok(false);
            }
            self.drain(Duration::from_millis(50));
        }
    }

    fn kill_group(&self, signal: libc::c_int) {
        let pid = self.child.id() as libc::pid_t;
        // SAFETY: signalling a process group this struct created and still owns.
        unsafe {
            libc::killpg(pid, signal);
        }
    }
}

/// A poll that errors out mid-session must not leave a Claude Code TUI running
/// in the background on its own pty.
impl Drop for PtyChild {
    fn drop(&mut self) {
        if matches!(self.child.try_wait(), Ok(None)) {
            self.kill_group(libc::SIGKILL);
            // Bounded and draining, for the same reason `finish` is: a
            // blocking wait here could hang the whole poll on the way out.
            let _ = self.reap(Duration::from_secs(5));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_child_sees_a_terminal_and_receives_typed_input() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out.txt");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "if [ -t 0 ]; then echo tty >> '{0}'; fi; read line; echo \"$line\" >> '{0}'",
            out.display()
        ));
        let mut pty = spawn(command, 80, 24).unwrap();
        pty.drain(Duration::from_millis(300));
        pty.write_all(b"hello\r").unwrap();
        pty.finish(Duration::from_secs(5)).unwrap();
        let text = std::fs::read_to_string(&out).unwrap();
        assert_eq!(text, "tty\nhello\n");
    }

    #[test]
    fn the_terminal_tail_is_kept_as_readable_text() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("done");
        // A coloured, cursor-moving line of the sort a TUI actually emits.
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "printf '\\033[2J\\033[1;36mAuto-updating Claude Code\\033[0m\\r\\n'; \
             printf 'second line\\r\\n'; : > '{}'",
            out.display()
        ));
        let mut pty = spawn(command, 80, 24).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !out.exists() && Instant::now() < deadline {
            pty.drain(Duration::from_millis(50));
        }
        pty.drain(Duration::from_millis(200));
        let text = pty.recent_text();
        let _ = pty.finish(Duration::from_secs(5));
        assert!(
            text.contains("Auto-updating Claude Code"),
            "the words must survive the escapes: {text:?}"
        );
        assert!(text.contains("second line"), "{text:?}");
        assert!(
            !text.contains('\u{1b}'),
            "no escape bytes may reach a log line: {text:?}"
        );
    }

    #[test]
    fn the_kept_tail_cannot_grow_without_bound() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("done");
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "i=0; while [ $i -lt 4000 ]; do echo 'noisy line of terminal output'; \
             i=$((i+1)); done; : > '{}'",
            out.display()
        ));
        let mut pty = spawn(command, 80, 24).unwrap();
        let deadline = Instant::now() + Duration::from_secs(20);
        while !out.exists() && Instant::now() < deadline {
            pty.drain(Duration::from_millis(50));
        }
        pty.drain(Duration::from_millis(200));
        assert!(
            pty.recent.len() <= RECENT_CAP,
            "kept {} bytes, cap is {RECENT_CAP}",
            pty.recent.len()
        );
        let _ = pty.finish(Duration::from_secs(5));
    }

    #[test]
    fn the_master_is_close_on_exec_so_the_child_cannot_hold_it() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("probe.txt");
        // The child asks about the parent's master by number, which it reads
        // from its terminal once the parent knows it: after exec a
        // close-on-exec descriptor is simply not there. Nothing may open a
        // descriptor before that probe: listing /dev/fd opens the directory,
        // and a redirect around the probe makes the shell save fd 1 at the
        // first free number from 10 up. Under parallel tests either lands on
        // the freed number and reads as the master (seen 2026-09-06).
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "read fd; r=free; [ -e /dev/fd/$fd ] && r=held; echo $r > '{}'",
            out.display()
        ));
        let mut pty = spawn(command, 80, 24).unwrap();
        // The flag itself, on the parent's side of the same descriptor.
        // SAFETY: fcntl on a descriptor this test owns.
        let flags = unsafe { libc::fcntl(pty.master.as_raw_fd(), libc::F_GETFD) };
        assert!(
            flags >= 0 && flags & libc::FD_CLOEXEC != 0,
            "master is not close-on-exec"
        );
        pty.write_all(format!("{}\n", pty.master.as_raw_fd()).as_bytes())
            .unwrap();
        pty.finish(Duration::from_secs(5)).unwrap();
        assert_eq!(std::fs::read_to_string(&out).unwrap().trim(), "free");
    }

    #[test]
    fn finish_kills_the_whole_process_group_not_just_the_launcher() {
        let temp = tempfile::tempdir().unwrap();
        let pidfile = temp.path().join("grandchild.pid");
        // A launcher that waits on a long-lived grandchild, like `ovm claude`.
        let mut command = Command::new("sh");
        command.arg("-c").arg(format!(
            "sh -c 'echo $$ > \"{}\"; sleep 60' ; wait",
            pidfile.display()
        ));
        let mut pty = spawn(command, 80, 24).unwrap();
        let deadline = Instant::now() + Duration::from_secs(5);
        while !pidfile.is_file() && Instant::now() < deadline {
            pty.drain(Duration::from_millis(50));
        }
        let grandchild: i32 = std::fs::read_to_string(&pidfile)
            .unwrap()
            .trim()
            .parse()
            .unwrap();
        pty.finish(Duration::from_millis(200)).unwrap();
        std::thread::sleep(Duration::from_millis(200));
        // SAFETY: signal 0 only probes for existence.
        let alive = unsafe { libc::kill(grandchild, 0) } == 0;
        assert!(!alive, "grandchild {grandchild} survived finish()");
    }

    #[test]
    fn dropping_a_live_session_kills_it() {
        let temp = tempfile::tempdir().unwrap();
        let pidfile = temp.path().join("child.pid");
        let mut command = Command::new("sh");
        command
            .arg("-c")
            .arg(format!("echo $$ > '{}'; sleep 60", pidfile.display()));
        let pid = {
            let mut pty = spawn(command, 80, 24).unwrap();
            let deadline = Instant::now() + Duration::from_secs(5);
            while !pidfile.is_file() && Instant::now() < deadline {
                pty.drain(Duration::from_millis(50));
            }
            pty.child.id() as i32
        };
        std::thread::sleep(Duration::from_millis(100));
        // SAFETY: signal 0 only probes for existence.
        let alive = unsafe { libc::kill(pid, 0) } == 0;
        assert!(!alive, "child {pid} survived drop");
    }

    /// With the parent holding a slave, the master never hangs up and a
    /// session leader can sit in exit forever. The child's exit must be the
    /// hangup, promptly, and `finish` must return in the same breath.
    #[test]
    fn a_child_that_exits_hangs_up_the_master_and_finish_returns_at_once() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        let mut pty = spawn(command, 80, 24).unwrap();
        // `drain` sleeps out its budget once the terminal hangs up, so it is
        // the hangup that is asserted here, not the time.
        pty.drain(Duration::from_millis(500));
        assert!(
            pty.hung_up(),
            "the master did not hang up after the child exited"
        );
        let started = Instant::now();
        pty.finish(Duration::from_secs(5)).unwrap();
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "finish waited out a grace period on a child that was already gone"
        );
    }

    /// The deadlock: a child with more output pending than the terminal holds
    /// cannot exit until someone reads it. This one ignores the grace period,
    /// floods its terminal the moment it is told to leave, and only then
    /// exits — the shape of a TUI restoring the screen on SIGTERM. `finish`
    /// must keep reading while it waits, so it runs on its own thread here: a
    /// regression hangs that thread and the test fails on the clock instead
    /// of hanging cargo.
    #[test]
    fn finish_drains_the_terminal_so_a_child_that_floods_it_on_exit_can_go() {
        let mut command = Command::new("sh");
        command.arg("-c").arg(
            "trap 'dd if=/dev/zero bs=65536 count=16 2>/dev/null | tr \"\\0\" x; exit 0' TERM; \
             sleep 30",
        );
        let mut pty = spawn(command, 80, 24).unwrap();
        pty.drain(Duration::from_millis(200));
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let outcome = pty.finish(Duration::from_millis(200));
            let _ = tx.send(outcome);
        });
        match rx.recv_timeout(Duration::from_secs(12)) {
            Ok(outcome) => outcome.unwrap(),
            Err(_) => panic!("finish() hung on a child blocked writing to its terminal"),
        }
    }

    #[test]
    fn finish_kills_a_child_that_ignores_the_grace_period() {
        let mut command = Command::new("sh");
        command.arg("-c").arg("sleep 30");
        let started = Instant::now();
        let mut pty = spawn(command, 80, 24).unwrap();
        pty.finish(Duration::from_millis(200)).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
