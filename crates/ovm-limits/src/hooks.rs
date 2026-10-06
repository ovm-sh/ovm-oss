//! Commands of the person's choosing, run when something happens. A hook is
//! a shell line; it gets the event (or the poll) described in its environment
//! and, for events, as JSON on stdin. Hooks never fail a poll: a notifier
//! that is down is logged and the numbers still land.

use crate::events::Event;
use crate::paths::LimitsDirs;
use crate::registry::Registry;
use console::style;
use serde::{Deserialize, Serialize};
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

/// Generous on purpose: a hook is a curl, but the machine that runs it may
/// be compiling something, and a push that lands late beats one that is
/// killed on the way out.
const TIMEOUT: Duration = Duration::from_secs(90);

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Hooks {
    /// Runs once per event, with the event as JSON on stdin.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_event: Option<String>,
    /// Runs once after every poll that polled anything, once the merged
    /// files are written.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_poll: Option<String>,
    /// Runs right after `on_poll`, with the same environment. Kept apart so a
    /// digest (one line per account to a phone) can sit next to a publish
    /// step without either having to know about the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on_digest: Option<String>,
}

impl Hooks {
    pub fn is_empty(&self) -> bool {
        self.on_event.is_none() && self.on_poll.is_none() && self.on_digest.is_none()
    }

    pub fn get(&self, name: &str) -> Option<Option<&str>> {
        match name {
            "on-event" | "on_event" => Some(self.on_event.as_deref()),
            "on-poll" | "on_poll" => Some(self.on_poll.as_deref()),
            "on-digest" | "on_digest" => Some(self.on_digest.as_deref()),
            _ => None,
        }
    }

    /// Set or clear a hook by name. `None` when the name is not a hook.
    pub fn set(&mut self, name: &str, command: Option<String>) -> Option<()> {
        match name {
            "on-event" | "on_event" => self.on_event = command,
            "on-poll" | "on_poll" => self.on_poll = command,
            "on-digest" | "on_digest" => self.on_digest = command,
            _ => return None,
        }
        Some(())
    }
}

/// What every hook can read from its environment.
fn common_env(dirs: &LimitsDirs) -> Vec<(String, String)> {
    vec![
        (
            "OVM_LIMITS_FILE".into(),
            dirs.merged_file().to_string_lossy().into_owned(),
        ),
        (
            "OVM_LIMITS_PUBLIC_FILE".into(),
            dirs.public_file().to_string_lossy().into_owned(),
        ),
        (
            "OVM_LIMITS_EVENTS_FILE".into(),
            dirs.events_file().to_string_lossy().into_owned(),
        ),
    ]
}

/// Run `on_event` for each event, in order.
pub fn on_events(dirs: &LimitsDirs, registry: &Registry, events: &[Event]) {
    let Some(command) = registry.hooks.on_event.as_deref() else {
        return;
    };
    for event in events {
        let mut env = common_env(dirs);
        let json = serde_json::to_string(event).unwrap_or_default();
        env.push(("OVM_LIMITS_EVENT".into(), json.clone()));
        env.push(("OVM_LIMITS_EVENT_TEXT".into(), event.text.clone()));
        env.push((
            "OVM_LIMITS_EVENT_KIND".into(),
            serde_json::to_value(event.kind)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default(),
        ));
        env.push(("OVM_LIMITS_ACCOUNT".into(), event.account_id.clone()));
        env.push((
            "OVM_LIMITS_LABEL".into(),
            event.label.clone().unwrap_or_default(),
        ));
        env.push((
            "OVM_LIMITS_WINDOW".into(),
            event.window_label.clone().unwrap_or_default(),
        ));
        // The threshold that was crossed, so a hook can tell 70% from 100%
        // without parsing the text or reaching into the JSON. Empty for every
        // other kind.
        env.push((
            "OVM_LIMITS_LEVEL".into(),
            event.level.map(|l| l.to_string()).unwrap_or_default(),
        ));
        run("on-event", command, &env, Some(json.as_bytes()));
    }
}

/// Run `on_poll` once, then `on_digest` if its own interval has come round.
pub fn on_poll(dirs: &LimitsDirs, registry: &Registry, polled: usize, events: usize) {
    let hooks = &registry.hooks;
    if hooks.on_poll.is_none() && hooks.on_digest.is_none() {
        return;
    }
    let mut env = common_env(dirs);
    env.push(("OVM_LIMITS_POLLED".into(), polled.to_string()));
    env.push(("OVM_LIMITS_EVENTS".into(), events.to_string()));
    if let Some(command) = hooks.on_poll.as_deref() {
        run("on-poll", command, &env, None);
    }
    if let Some(command) = hooks.on_digest.as_deref() {
        let now = crate::snapshot::now();
        if !digest_is_due(dirs, registry.digest_minutes, now) {
            return;
        }
        // Stamped before the run, not after: a digest that takes a minute must
        // not push the next one a minute later every time, and a hook that
        // fails should wait its turn like any other rather than retrying on
        // every poll.
        write_last_digest(dirs, now);
        run("on-digest", command, &env, None);
    }
}

/// Has `digest_minutes` passed since the digest last ran? A missing or
/// unreadable stamp means yes — the first digest after this ships, or after
/// someone clears the file, goes out at once.
fn digest_is_due(dirs: &LimitsDirs, digest_minutes: u64, now: u64) -> bool {
    if digest_minutes == 0 {
        return true;
    }
    let Some(last) = read_last_digest(dirs) else {
        return true;
    };
    // A stamp in the future is a clock that moved backwards; treat it as due
    // rather than going silent until the clock catches up.
    now < last || now.saturating_sub(last) >= digest_minutes.saturating_mul(60)
}

fn read_last_digest(dirs: &LimitsDirs) -> Option<u64> {
    std::fs::read_to_string(dirs.last_digest_file())
        .ok()?
        .trim()
        .parse()
        .ok()
}

fn write_last_digest(dirs: &LimitsDirs, now: u64) {
    if let Err(error) = std::fs::write(dirs.last_digest_file(), format!("{now}\n")) {
        eprintln!(
            "  {} could not record the digest time: {error}",
            style("!").yellow()
        );
    }
}

/// `sh -c command`, bounded, output kept for the log line. Returns whether it
/// succeeded, for `hook test`.
pub fn run(name: &str, command: &str, env: &[(String, String)], stdin: Option<&[u8]>) -> bool {
    let mut child = match Command::new("sh")
        .arg("-c")
        .arg(command)
        .envs(env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(error) => {
            eprintln!(
                "  {} hook {name} could not start: {error}",
                style("!").yellow()
            );
            return false;
        }
    };
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        let _ = pipe.write_all(bytes);
    }
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().ok();
                if status.success() {
                    return true;
                }
                let stderr = output
                    .map(|o| String::from_utf8_lossy(&o.stderr).trim().to_string())
                    .unwrap_or_default();
                eprintln!(
                    "  {} hook {name} exited with {status}{}",
                    style("!").yellow(),
                    if stderr.is_empty() {
                        String::new()
                    } else {
                        format!(": {stderr}")
                    }
                );
                return false;
            }
            Ok(None) if started.elapsed() < TIMEOUT => {
                std::thread::sleep(Duration::from_millis(50));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                eprintln!(
                    "  {} hook {name} took longer than {}s and was stopped",
                    style("!").yellow(),
                    TIMEOUT.as_secs()
                );
                return false;
            }
            Err(error) => {
                eprintln!("  {} hook {name}: {error}", style("!").yellow());
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hook_sees_its_environment_and_its_stdin() {
        let temp = tempfile::tempdir().unwrap();
        let out = temp.path().join("out.txt");
        let command = format!("cat > '{}'; echo \"$X\" >> '{0}'", out.display());
        let env = vec![("X".to_string(), "marker".to_string())];
        assert!(run("test", &command, &env, Some(b"payload\n")));
        assert_eq!(std::fs::read_to_string(&out).unwrap(), "payload\nmarker\n");
    }

    #[test]
    fn a_failing_hook_reports_false_and_never_panics() {
        assert!(!run("test", "exit 3", &[], None));
        assert!(!run("test", "/nonexistent/binary", &[], None));
    }

    #[test]
    fn the_digest_waits_its_interval_and_a_backwards_clock_does_not_silence_it() {
        let temp = tempfile::tempdir().unwrap();
        let dirs = LimitsDirs::at(temp.path().to_path_buf());
        // Never run before.
        assert!(digest_is_due(&dirs, 60, 10_000));
        write_last_digest(&dirs, 10_000);
        assert!(!digest_is_due(&dirs, 60, 10_000 + 59 * 60));
        assert!(digest_is_due(&dirs, 60, 10_000 + 60 * 60));
        // Zero means every poll, the behaviour before the interval existed.
        assert!(digest_is_due(&dirs, 0, 10_001));
        // A clock that went backwards must not mute the digest indefinitely.
        assert!(digest_is_due(&dirs, 60, 9_000));
    }

    #[test]
    fn hooks_are_addressed_by_name() {
        let mut hooks = Hooks::default();
        assert!(hooks.is_empty());
        assert!(hooks.set("on-event", Some("true".into())).is_some());
        assert!(hooks.set("on_poll", Some("true".into())).is_some());
        assert!(hooks.set("on-digest", Some("true".into())).is_some());
        assert!(hooks.set("on-launch", Some("true".into())).is_none());
        assert_eq!(hooks.get("on_digest"), Some(Some("true")));
        assert_eq!(hooks.get("on-event"), Some(Some("true")));
        hooks.set("on-event", None);
        assert_eq!(hooks.get("on-event"), Some(None));
        assert_eq!(hooks.get("nope"), None);
    }
}
