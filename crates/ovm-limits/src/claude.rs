//! Polling a Claude account the only way that keeps the credential inside
//! Claude Code: run Claude Code.
//!
//! Anthropic's usage windows are not published anywhere a third party may
//! read them — consumer OAuth tokens and claude.ai session cookies are for
//! Claude Code and claude.ai alone. Claude Code does hand the windows to its
//! own statusline script (`rate_limits.five_hour` / `.seven_day` /
//! `.spend_limit`), but only in the interactive TUI and only after the first
//! assistant reply of a session. So a poll is: start a throwaway TUI session
//! in an empty directory with a statusline that appends its payload to a
//! file, type one word, wait for a payload carrying `rate_limits`, `/exit`.
//!
//! The token never leaves the unmodified Claude Code binary and the traffic
//! is one ordinary turn on the cheapest model.

use crate::paths::LimitsDirs;
use crate::pty;
use crate::registry::{Account, Provider, DEFAULT_CLAUDE_MODEL};
use crate::snapshot::{self, AccountSnapshot, Window};
use crate::{LimitsError, Result};
use serde_json::{json, Value};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// Override the launcher (tests point this at a fake). Invoked directly with
/// the Claude arguments; the default goes through `ovm claude` so the poll
/// runs the version OVM has selected.
pub const LAUNCHER_ENV: &str = "OVM_LIMITS_CLAUDE_CMD";

pub const SOURCE: &str = "statusline";
const PROMPT: &str = "reply with just: ok";

#[derive(Debug, Clone, Copy)]
pub struct Timeouts {
    /// Session start until the first statusline payload (the TUI is up).
    pub ready: Duration,
    /// From typing the prompt until a payload carries `rate_limits`.
    pub limits: Duration,
    /// After `/exit` before the child is killed.
    pub exit: Duration,
}

impl Default for Timeouts {
    fn default() -> Self {
        Self {
            // Ninety, not forty-five: on 2026-09-24 the M4 sat at a load
            // average of 78 on 14 cores (another project's job forking an AWS
            // CLI process per S3 key) and Claude Code intermittently needed
            // more than 45s just to take over the terminal — sixteen polls
            // failed that day with nothing wrong with the account.
            ready: Duration::from_secs(90),
            limits: Duration::from_secs(90),
            exit: Duration::from_secs(8),
        }
    }
}

pub fn poll(account: &Account, dirs: &LimitsDirs, timeouts: Timeouts) -> Result<AccountSnapshot> {
    dirs.ensure_layout()?;
    let home = account.poll_home(dirs);
    refuse_default_home(&home)?;
    std::fs::create_dir_all(&home)?;
    let scratch = dirs.scratch_dir();
    seed_claude_json(&home.join(".claude.json"), &scratch)?;

    let capture = dirs.capture_file(&account.id);
    if let Some(parent) = capture.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&capture, b"")?;

    let mut command = launcher();
    command.args(claude_args(account, &capture));
    command.current_dir(&scratch);
    command.env("TERM", "xterm-256color");
    command.env(account.provider.home_env(), &home);

    let mut session = pty::spawn(command, 120, 40)
        .map_err(|error| LimitsError::Message(format!("couldn't start claude: {error}")))?;

    // Phase 1: the statusline runs once at session start — that is "ready".
    // A home of our own is a home Claude Code has never run in, so the session
    // can open on a one-time interstitial instead. The trust dialog is
    // pre-answered above and the onboarding flags are seeded, but others exist
    // ("Claude in Chrome extension detected", seen 2026-09-06) and none of them
    // reach the statusline. Escape is the documented "keep the default" on
    // every one of them, so the wait presses it a few times rather than sitting
    // out the timeout. Escape only ever declines: it can dismiss a prompt, it
    // can never accept one.
    dismiss_interstitials(&mut session, &capture, timeouts.ready)
        .map_err(|_| never_started(&session))?;
    session.drain(Duration::from_millis(1500));

    // Phase 2: one word in, one assistant reply out, and the payload that
    // follows it carries the windows. A turn the API refuses — the account is
    // out of quota, or its login has lapsed — still runs the statusline, but
    // with no windows in the payload; the refusal itself only reaches the
    // session transcript, which the first payload names. Watching both means
    // a refused turn is reported in the product's own words within a second
    // instead of as a timeout blamed on the wrong thing.
    //
    // Since 2.1.270 a hit limit can instead open a dialog ("What do you want
    // to do? 1. Stop and wait for limit to reset / 2. Wait here, then continue
    // automatically at … / 3. Switch to usage credits"), and while it is up
    // the transcript is not written at all — seen live on 2026-09-15 — so the
    // screen is watched as well. The dialog is declined with Escape before the
    // session is left: Escape can only ever cancel it, never pick "switch to
    // usage credits", which is money.
    let transcript = transcript_path(&capture);
    session.write_all(format!("{PROMPT}\r").as_bytes())?;
    let outcome = wait_for(&mut session, timeouts.limits, |session| {
        if let Some(payload) = latest_payload_with_rate_limits(&capture) {
            return Some(Turn::Answered(payload));
        }
        latest_refusal(transcript.as_deref())
            .or_else(|| refusal_on_screen(&session.recent_text()))
            .map(Turn::Refused)
    })
    .map_err(|_| LimitsError::Message(explain_no_limits(&session.recent_text())));
    let payload = match outcome {
        Ok(Turn::Answered(payload)) => payload,
        Ok(Turn::Refused(refusal)) => {
            let _ = session.write_all(b"\x1b");
            session.drain(Duration::from_millis(500));
            let _ = session.write_all(b"/exit\r");
            let _ = session.finish(timeouts.exit);
            return Err(LimitsError::Message(explain_refusal(account, &refusal)));
        }
        Err(error) => return Err(error),
    };

    // Phase 3: leave politely, then insist.
    let _ = session.write_all(b"/exit\r");
    session.finish(timeouts.exit)?;

    let mut snapshot = snapshot_from_payload(account, &payload);
    snapshot.account_id = login_account_uuid(&home);
    Ok(snapshot)
}

/// The login a home holds: `oauthAccount.accountUuid` in its `.claude.json`.
///
/// Live readings (Echo in a natural session) and polls are matched on this,
/// not on the folder: `/login` can switch the account inside one home, and a
/// statusline payload names no account at all. `None` when the file or the
/// field is missing — a reading that cannot be tied to a login is not merged.
pub fn login_account_uuid(home: &Path) -> Option<String> {
    let raw = std::fs::read_to_string(claude_json_path(home)).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    doc.pointer("/oauthAccount/accountUuid")
        .and_then(Value::as_str)
        .filter(|uuid| !uuid.is_empty())
        .map(str::to_owned)
}

/// What kind of login a home holds and whose: read from `.claude.json`'s
/// `oauthAccount`, never from the Keychain.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct LoginDetails {
    /// `team` for a seat in an organisation's plan, `personal` for an own plan.
    pub kind: String,
    pub org: Option<String>,
    pub org_id: Option<String>,
    /// The plan tier the login reports (`default_claude_max_20x`,
    /// `team_standard`…): the organisation's, else the user's.
    pub rate_limit_tier: Option<String>,
}

/// The same rule `ovm accounts` uses: a seat tier, or a team / enterprise
/// organisation type, makes it a team seat.
pub fn login_details(home: &Path) -> Option<LoginDetails> {
    let raw = std::fs::read_to_string(claude_json_path(home)).ok()?;
    let doc: Value = serde_json::from_str(&raw).ok()?;
    let account = doc.get("oauthAccount")?;
    let text = |key: &str| {
        account
            .get(key)
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
            .map(str::to_owned)
    };
    let team = text("seatTier").is_some()
        || text("organizationType").is_some_and(|t| t.contains("team") || t.contains("enterprise"));
    Some(LoginDetails {
        kind: if team { "team" } else { "personal" }.into(),
        org: text("organizationName"),
        org_id: text("organizationUuid"),
        rate_limit_tier: text("organizationRateLimitTier").or_else(|| text("userRateLimitTier")),
    })
}

/// The error for a session that never reached its statusline, named from what
/// the terminal actually showed.
///
/// This used to be one fixed sentence blaming a sign-out or a first-run
/// screen. On the night of 2026-09-08 it was printed twelve times and was
/// wrong every time: the cause was an inline auto-update download, and later
/// something else again, with both homes signed in throughout. A poll that
/// reports the same guess whatever happened cannot be diagnosed after the
/// fact, so the message now says what was on screen and keeps the tail for
/// whoever reads the log.
fn never_started(session: &pty::PtyChild) -> LimitsError {
    LimitsError::Message(explain_never_started(&session.recent_text()))
}

/// Pure, so the classification can be tested without a terminal.
fn explain_never_started(screen: &str) -> String {
    let lower = screen.to_lowercase();
    let cause = if only_our_escapes(screen) {
        // The terminal is still in cooked mode, echoing this poll's own Escape
        // presses back as `^[`: the product never took over the screen. That
        // is a slow start, not a dialog, and it used to be reported (and
        // pushed to a phone) as "a screen this poll could not dismiss".
        return format!(
            "claude never ran the statusline — Claude Code had not started after the \
             wait (only this poll's own Escape presses echoed back); the machine was \
             probably too busy{}",
            load_note()
        );
    } else if lower.contains("auto-updating") || lower.contains("downloading") {
        "ovm is installing a newer build of the product, which takes longer than this \
         poll may wait — it should recover once the install finishes"
    } else if lower.contains("log in") || lower.contains("sign in") || lower.contains("/login") {
        "the session is asking to be signed in — run: ovm limits login <account>"
    } else if screen.trim().is_empty() {
        "the session printed nothing at all — the product may not have started"
    } else {
        "a screen this poll could not dismiss is blocking the session"
    };
    with_screen_tail(format!("claude never ran the statusline — {cause}"), screen)
}

/// The error for a turn that neither answered with windows nor was refused
/// within the wait. Before 2026-09-15 this was the message for every account
/// that had hit its weekly limit: the API turned the turn away, the statusline
/// ran without windows, and the poll waited out its 90s to blame the plan.
/// Refusals are now caught by [`explain_refusal`]; what is left here is the
/// genuinely unexplained case, so the screen is quoted for whoever reads it.
fn explain_no_limits(screen: &str) -> String {
    with_screen_tail(
        "the session replied without rate limits — this only appears for Claude.ai \
         Pro/Max logins (or behind an apps gateway); an API-key login has no windows"
            .into(),
        screen,
    )
}

/// Nothing on screen but the `^[` a cooked-mode terminal echoes for Escape.
fn only_our_escapes(screen: &str) -> bool {
    screen.contains("^[") && screen.replace("^[", "").trim().is_empty()
}

/// `; load average 78.6 on 14 cores` — the one number that tells a slow start
/// on a busy machine from a broken product. Empty when it cannot be read.
fn load_note() -> String {
    let mut loads = [0f64; 3];
    // SAFETY: getloadavg writes at most `nelem` doubles into the buffer.
    let read = unsafe { libc::getloadavg(loads.as_mut_ptr(), 3) };
    if read < 1 {
        return String::new();
    }
    let cores = std::thread::available_parallelism().map_or(0, std::num::NonZeroUsize::get);
    format!("; load average {:.1} on {cores} cores", loads[0])
}

/// Terminal bytes that carry no words: the `^[` Escape echo and the block and
/// box-drawing glyphs of Claude Code's logo. Quoted as-is they reached a phone
/// as `last on screen: ▝▜█████ ^[^[`.
fn is_screen_noise(ch: char) -> bool {
    matches!(ch, '\u{2500}'..='\u{259f}' | '\u{23ce}')
}

/// Append the last few non-blank lines of the terminal to a message. Blank
/// lines carry nothing and would quote an empty screen back as a row of
/// separators, so they never reach the message.
fn with_screen_tail(message: String, screen: &str) -> String {
    let cleaned: String = screen
        .replace("^[", "")
        .chars()
        .filter(|ch| !is_screen_noise(*ch))
        .collect();
    let mut tail: Vec<&str> = cleaned
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .rev()
        .take(6)
        .collect();
    tail.reverse();
    let tail = tail.join(" ⏎ ").chars().take(400).collect::<String>();
    if tail.is_empty() {
        return message;
    }
    format!("{message} (last on screen: {tail})")
}

/// How the poll's one turn ended.
enum Turn {
    /// The statusline payload carrying the windows.
    Answered(Value),
    /// The API turned the turn away; the product's own words.
    Refused(Refusal),
}

/// A turn Claude Code could not complete. The product writes such a turn to
/// the session transcript as a synthetic assistant message flagged
/// `isApiErrorMessage`, with `error` naming the class and the text the TUI
/// shows. Seen so far: `rate_limit` ("You've hit your weekly limit · resets
/// Sep 17 at 10pm (Europe/Lisbon)") and `authentication_failed` ("Failed to
/// authenticate: OAuth session expired and could not be refreshed").
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    pub kind: String,
    pub text: String,
}

/// The transcript this session writes, as named by its first statusline
/// payload. `None` until that payload exists, or on a build that omits it.
fn transcript_path(capture: &Path) -> Option<PathBuf> {
    payloads(capture).iter().find_map(|payload| {
        payload
            .get("transcript_path")
            .and_then(Value::as_str)
            .map(PathBuf::from)
    })
}

fn latest_refusal(transcript: Option<&Path>) -> Option<Refusal> {
    let raw = std::fs::read_to_string(transcript?).ok()?;
    refusal_from_transcript(&raw)
}

/// Pure: the newest refused turn in a transcript, if any. A session lives in
/// its own transcript, so any refusal found is this poll's.
pub fn refusal_from_transcript(raw: &str) -> Option<Refusal> {
    raw.lines()
        .rev()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find_map(|entry| {
            if entry.get("isApiErrorMessage").and_then(Value::as_bool) != Some(true) {
                return None;
            }
            let kind = entry
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("api_error")
                .to_owned();
            let text = entry
                .pointer("/message/content")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .collect::<Vec<_>>()
                        .join(" ")
                })
                .unwrap_or_default();
            Some(Refusal { kind, text })
        })
}

/// Pure: a hit limit as the terminal shows it — the dialog Claude Code opens
/// when the API turns a turn away, or the one-line notice older builds print.
/// The quoted text is the line that carries the reset time when there is one.
pub fn refusal_on_screen(screen: &str) -> Option<Refusal> {
    const SIGNS: [&str; 4] = [
        "hit your",
        "continue automatically at",
        "wait for limit to reset",
        "usage limit reached",
    ];
    let lines: Vec<&str> = screen
        .lines()
        .map(|line| {
            line.trim()
                .trim_start_matches(|c: char| {
                    matches!(c, '❯' | '⎿' | '⏺' | '.' | ' ') || c.is_ascii_digit()
                })
                .trim()
        })
        .filter(|line| !line.is_empty())
        .collect();
    let has = |sign: &str| lines.iter().any(|line| line.to_lowercase().contains(sign));
    // The notice alone names the limit; a dialog needs both of its first two
    // options before it counts, so one word on an unrelated screen cannot.
    let notice = has("hit your") && has("limit");
    let dialog = has("wait for limit to reset") && has("continue automatically at");
    if !notice && !dialog && !has("usage limit reached") {
        return None;
    }
    let text = SIGNS
        .iter()
        .find_map(|sign| lines.iter().find(|line| line.to_lowercase().contains(sign)))
        .map(|line| line.to_string())
        .unwrap_or_default();
    Some(Refusal {
        kind: "rate_limit".into(),
        text,
    })
}

/// Pure: the error for a refused turn, from the product's own words.
fn explain_refusal(account: &Account, refusal: &Refusal) -> String {
    let quoted = if refusal.text.is_empty() {
        String::new()
    } else {
        format!(": \"{}\"", refusal.text)
    };
    match refusal.kind.as_str() {
        "rate_limit" => format!(
            "out of quota — Claude Code refused the turn{quoted}; the windows come back with \
             the first turn that goes through, so this poll recovers after that reset"
        ),
        "authentication_failed" => format!(
            "signed out — Claude Code refused the turn{quoted} — run: ovm limits login {}",
            account.id
        ),
        kind => format!("Claude Code refused the turn ({kind}){quoted}"),
    }
}

/// Wait for the first statusline payload, pressing Escape at intervals while
/// it does not come. Each press declines whatever one-time screen is up; a
/// session that is merely slow is unaffected, because Escape at an idle prompt
/// does nothing.
fn dismiss_interstitials(
    session: &mut pty::PtyChild,
    capture: &Path,
    timeout: Duration,
) -> std::result::Result<(), TimedOut> {
    const ESCAPE: &[u8] = b"\x1b";
    const ATTEMPTS: usize = 4;
    let slice = timeout / (ATTEMPTS as u32 + 1);
    for attempt in 0..=ATTEMPTS {
        if wait_until(session, slice, || payload_count(capture) >= 1).is_ok() {
            return Ok(());
        }
        if session.hung_up() {
            return Err(TimedOut);
        }
        if attempt < ATTEMPTS {
            let _ = session.write_all(ESCAPE);
        }
    }
    Err(TimedOut)
}

fn launcher() -> Command {
    match std::env::var_os(LAUNCHER_ENV).filter(|value| !value.is_empty()) {
        Some(path) => Command::new(path),
        // A poll has 45s to reach the statusline; an inline auto-update
        // download takes minutes, and the kill that ends the timed-out poll
        // stops the failure ever being recorded, so the next poll repeats it.
        // The poll runs whatever version is active and leaves upgrading to a
        // launch that is not on a clock.
        None => crate::ovm_launcher("claude"),
    }
}

/// The arguments handed to Claude Code for one poll. Public so tests can pin
/// the contract without spawning anything.
pub fn claude_args(account: &Account, capture: &Path) -> Vec<String> {
    let model = account
        .model
        .clone()
        .unwrap_or_else(|| DEFAULT_CLAUDE_MODEL.to_string());
    vec![
        "--model".into(),
        model,
        "--settings".into(),
        capture_settings(capture),
        // No MCP servers: they only inflate the system prompt this poll pays for.
        "--strict-mcp-config".into(),
    ]
}

/// A statusline that appends each payload to `capture`, newline-terminated,
/// leaving the account's own settings.json untouched (`--settings` layers on
/// top for this process only).
fn capture_settings(capture: &Path) -> String {
    let quoted = shell_single_quote(&capture.to_string_lossy());
    json!({
        "statusLine": {
            "type": "command",
            "command": format!("cat >> {quoted} && printf '\\n' >> {quoted}"),
        }
    })
    .to_string()
}

fn shell_single_quote(text: &str) -> String {
    format!("'{}'", text.replace('\'', "'\\''"))
}

/// A relocated home (`CLAUDE_CONFIG_DIR`) carries `.claude.json` inside it, so
/// an isolated poll home always keeps its own. The default login keeps the file
/// at `~/.claude.json`, beside — not inside — `~/.claude`, but polls never go
/// there; see `refuse_default_home`.
pub fn claude_json_path(home: &Path) -> PathBuf {
    home.join(".claude.json")
}

/// A poll drives a real Claude session, and a real session refreshes the
/// account's OAuth token. In the home you work in, that rotation invalidates
/// the token your own open sessions hold and signs them out — seen on
/// 2026-09-06. So the product's own home is refused outright.
pub fn refuse_default_home(home: &Path) -> Result<()> {
    if Provider::Claude.is_default_home(home) {
        return Err(LimitsError::Message(format!(
            "refusing to poll {}: that is the home your own Claude Code sessions use, and a poll \
             refreshes its login. Give the account a home of its own (drop `--home`, or point it \
             somewhere else) and sign in there once.",
            crate::paths::display(home)
        )));
    }
    Ok(())
}

/// Pre-accept the scratch directory in the account's `.claude.json`, the same
/// key Claude Code writes when a person clicks "Yes, I trust this folder", and
/// make sure the home counts as onboarded. Nothing else in the file is touched,
/// and a home that already has both means no write at all — so this costs one
/// write per home, on its first poll.
///
/// Claude Code rewrites this file as it runs, and an explicit `--home` may name
/// a home the person also works in. Rather than take a lock, the file is read
/// again immediately before the rename and the whole edit is redone if it moved
/// under us: a concurrent session's changes must not be replaced by our older
/// copy. That narrows the window to the microseconds between the second read
/// and the rename; it does not close it.
fn seed_claude_json(path: &Path, scratch: &Path) -> Result<()> {
    for _ in 0..4 {
        let before = read_if_present(path)?;
        let Some(seeded) = seeded_claude_json(before.as_deref(), path, scratch)? else {
            return Ok(());
        };
        if read_if_present(path)? != before {
            continue;
        }
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        return snapshot::write_atomic(path, seeded.as_bytes());
    }
    Err(LimitsError::Message(format!(
        "{} kept changing while it was being seeded — is a Claude session using this home?",
        crate::paths::display(path)
    )))
}

fn read_if_present(path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(path) {
        Ok(raw) => Ok(Some(raw)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

/// The file this home should have, or `None` when it already says everything
/// this poll needs. Pure, so the decision is testable without a filesystem.
fn seeded_claude_json(raw: Option<&str>, path: &Path, scratch: &Path) -> Result<Option<String>> {
    let mut root = match raw {
        Some(raw) if !raw.trim().is_empty() => serde_json::from_str::<Value>(raw).map_err(|e| {
            LimitsError::Message(format!("{} is not valid JSON ({e})", path.display()))
        })?,
        _ => json!({}),
    };
    let object = root
        .as_object_mut()
        .ok_or_else(|| LimitsError::Message(format!("{} is not a JSON object", path.display())))?;
    // `claude auth login` writes a file that holds the account but no
    // onboarding flags, so a home signed in through `ovm limits login` still
    // meets the first-run wizard on its first poll — and the wizard never
    // reaches the statusline (found 2026-09-06). Both flags are filled in
    // whenever either is missing, not only for a home with no file yet.
    let onboarded = object
        .get("hasCompletedOnboarding")
        .and_then(Value::as_bool)
        .unwrap_or(false)
        && object.contains_key("lastOnboardingVersion");
    object.insert("hasCompletedOnboarding".into(), Value::Bool(true));
    object
        .entry("lastOnboardingVersion")
        .or_insert_with(|| json!("2.1.0"));

    let projects = object.entry("projects").or_insert_with(|| json!({}));
    let projects = projects.as_object_mut().ok_or_else(|| {
        LimitsError::Message(format!("{}: `projects` is not an object", path.display()))
    })?;
    let project = projects
        .entry(scratch.to_string_lossy().into_owned())
        .or_insert_with(|| json!({}));
    let trusted = project
        .get("hasTrustDialogAccepted")
        .and_then(Value::as_bool)
        .unwrap_or(false);
    if let Some(project) = project.as_object_mut() {
        project.insert("hasTrustDialogAccepted".into(), Value::Bool(true));
    }
    if trusted && onboarded && raw.is_some() {
        return Ok(None);
    }
    Ok(Some(serde_json::to_string_pretty(&root)?))
}

/// Undo `seed_claude_json`: drop the scratch directory's project entry. Only
/// our throwaway sessions ever ran there, so the whole entry is ours to remove.
/// Returns whether anything changed.
pub fn unseed_claude_json(path: &Path, scratch: &Path) -> Result<bool> {
    let raw = match std::fs::read_to_string(path) {
        Ok(raw) => raw,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(false),
        Err(error) => return Err(error.into()),
    };
    let mut root: Value = match serde_json::from_str(&raw) {
        Ok(root) => root,
        Err(_) => return Ok(false),
    };
    let removed = root
        .get_mut("projects")
        .and_then(Value::as_object_mut)
        .and_then(|projects| projects.remove(scratch.to_string_lossy().as_ref()))
        .is_some();
    if removed {
        snapshot::write_atomic(path, serde_json::to_string_pretty(&root)?.as_bytes())?;
    }
    Ok(removed)
}

/// The command that reaches this account's Claude: the OVM-selected one, in the
/// account's own home. Used by `login`; polls build on it too.
pub fn product_command(account: &Account, home: &Path) -> Command {
    let mut command = launcher();
    command.env(account.provider.home_env(), home);
    command
}

struct TimedOut;

fn wait_until(
    session: &mut pty::PtyChild,
    timeout: Duration,
    mut condition: impl FnMut() -> bool,
) -> std::result::Result<(), TimedOut> {
    let deadline = Instant::now() + timeout;
    loop {
        if condition() {
            return Ok(());
        }
        if Instant::now() >= deadline || session.hung_up() {
            return Err(TimedOut);
        }
        session.drain(Duration::from_millis(250));
    }
}

fn wait_for<T>(
    session: &mut pty::PtyChild,
    timeout: Duration,
    mut probe: impl FnMut(&pty::PtyChild) -> Option<T>,
) -> std::result::Result<T, TimedOut> {
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(value) = probe(session) {
            return Ok(value);
        }
        if Instant::now() >= deadline || session.hung_up() {
            return Err(TimedOut);
        }
        session.drain(Duration::from_millis(250));
    }
}

fn payloads(capture: &Path) -> Vec<Value> {
    std::fs::read_to_string(capture)
        .map(|raw| {
            raw.lines()
                .filter(|line| !line.trim().is_empty())
                .filter_map(|line| serde_json::from_str(line).ok())
                .collect()
        })
        .unwrap_or_default()
}

fn payload_count(capture: &Path) -> usize {
    payloads(capture).len()
}

fn latest_payload_with_rate_limits(capture: &Path) -> Option<Value> {
    payloads(capture)
        .into_iter()
        .rev()
        .find(|payload| !windows_from_statusline(payload).is_empty())
}

/// Pure: statusline payload → windows. Each window may be independently
/// absent (Claude Code drops one once its `resets_at` passes).
pub fn windows_from_statusline(payload: &Value) -> Vec<Window> {
    const WINDOWS: [(&str, &str, Option<u64>); 3] = [
        ("five_hour", "5h", Some(300)),
        ("seven_day", "7d", Some(10_080)),
        ("spend_limit", "spend", None),
    ];
    let Some(limits) = payload.get("rate_limits").and_then(Value::as_object) else {
        return Vec::new();
    };
    WINDOWS
        .iter()
        .filter_map(|(id, label, minutes)| {
            let window = limits.get(*id)?;
            let used = window.get("used_percentage").and_then(Value::as_f64)?;
            Some(Window {
                id: (*id).into(),
                label: (*label).into(),
                used_percent: used,
                resets_at: window.get("resets_at").and_then(Value::as_u64),
                window_minutes: *minutes,
                last_reset_at: None,
            })
        })
        .collect()
}

/// Everything the statusline says about the account. The rest of the payload
/// describes the throwaway session (its context window, its prompt cache)
/// and is left in the capture file for diagnosis.
pub fn snapshot_from_payload(account: &Account, payload: &Value) -> AccountSnapshot {
    let text = |pointer: &str| {
        payload
            .pointer(pointer)
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    AccountSnapshot {
        windows: windows_from_statusline(payload),
        poll_cost_usd: payload
            .pointer("/cost/total_cost_usd")
            .and_then(Value::as_f64),
        model: text("/model/id"),
        product_version: text("/version"),
        raw: payload.get("rate_limits").cloned(),
        ..AccountSnapshot::for_account(account, SOURCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Provider;

    #[test]
    fn a_homes_login_is_read_from_its_claude_json() {
        let temp = tempfile::tempdir().unwrap();
        assert_eq!(login_account_uuid(temp.path()), None);
        std::fs::write(
            temp.path().join(".claude.json"),
            r#"{"oauthAccount":{"accountUuid":"a1e825aa-0000","emailAddress":"x"}}"#,
        )
        .unwrap();
        assert_eq!(
            login_account_uuid(temp.path()).as_deref(),
            Some("a1e825aa-0000")
        );
        std::fs::write(temp.path().join(".claude.json"), r#"{"oauthAccount":{}}"#).unwrap();
        assert_eq!(login_account_uuid(temp.path()), None);
    }

    fn account() -> Account {
        Account::new(Provider::Claude, "claude-1", None)
    }

    #[test]
    fn a_session_that_never_started_says_what_was_on_screen() {
        // The real thing, from the night this was written: the poll blamed a
        // sign-out while ovm was four minutes into a 200 MB download.
        let updating = explain_never_started(
            "  (≈^.^≈)  Auto-updating Claude Code 2.1.263 → 2.1.265\n  \
             → press Enter to skip and launch 2.1.263 now",
        );
        assert!(
            updating.contains("ovm is installing a newer build"),
            "an update in progress must be named as one: {updating}"
        );
        assert!(
            updating.contains("Auto-updating Claude Code"),
            "and the screen quoted back: {updating}"
        );
        assert!(
            !updating.contains("not signed in"),
            "it must not keep guessing at a sign-out: {updating}"
        );

        let signed_out = explain_never_started("Please sign in to continue\n/login");
        assert!(signed_out.contains("ovm limits login"), "{signed_out}");

        let silent = explain_never_started("   \n  \n");
        assert!(silent.contains("printed nothing at all"), "{silent}");
        assert!(
            !silent.contains("last on screen"),
            "an empty screen has no tail to quote: {silent}"
        );

        // 2026-09-24: the product had not started; the tty echoed our Escapes.
        let slow = explain_never_started("^[^[^[^[\n");
        assert!(slow.contains("had not started"), "{slow}");
        assert!(!slow.contains("could not dismiss"), "{slow}");
        assert!(!slow.contains("^["), "{slow}");

        // Logo glyphs and escape echoes never reach the quoted tail.
        let logo = explain_never_started(
            "^[\u{259d}\u{259c}\u{2588}\u{2588} Claude Code v2.1.277 \u{23ce}\n^[",
        );
        assert!(logo.contains("Claude Code v2.1.277"), "{logo}");
        assert!(!logo.contains('\u{2588}') && !logo.contains("^["), "{logo}");

        let unknown = explain_never_started("Some interstitial nobody has seen before");
        assert!(unknown.contains("could not dismiss"), "{unknown}");
        assert!(unknown.contains("last on screen"), "{unknown}");
    }

    #[test]
    fn a_refused_turn_is_read_from_the_transcript_in_the_products_words() {
        // Verbatim shapes from the poll homes on 2026-09-15, trimmed to the
        // fields that matter: the synthetic assistant message Claude Code
        // writes when the API turns a turn away.
        let transcript = concat!(
            r#"{"type":"user","message":{"role":"user","content":"reply with just: ok"}}"#,
            "\n",
            r#"{"type":"assistant","message":{"role":"assistant","model":"<synthetic>","content":[{"type":"text","text":"You've hit your weekly limit · resets Sep 17 at 10pm (Europe/Lisbon)"}]},"error":"rate_limit","isApiErrorMessage":true}"#,
            "\n",
            r#"{"type":"system","subtype":"turn_duration"}"#,
            "\n",
        );
        let refusal = refusal_from_transcript(transcript).expect("a refusal");
        assert_eq!(refusal.kind, "rate_limit");
        assert_eq!(
            refusal.text,
            "You've hit your weekly limit · resets Sep 17 at 10pm (Europe/Lisbon)"
        );
        let message = explain_refusal(&account(), &refusal);
        assert!(message.starts_with("out of quota"), "{message}");
        assert!(message.contains("resets Sep 17 at 10pm"), "{message}");

        let signed_out = refusal_from_transcript(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"Failed to authenticate: OAuth session expired and could not be refreshed"}]},"error":"authentication_failed","isApiErrorMessage":true}"#,
        )
        .unwrap();
        let message = explain_refusal(&account(), &signed_out);
        assert!(message.starts_with("signed out"), "{message}");
        assert!(message.contains("ovm limits login claude-1"), "{message}");

        let unknown = Refusal {
            kind: "overloaded".into(),
            text: String::new(),
        };
        assert_eq!(
            explain_refusal(&account(), &unknown),
            "Claude Code refused the turn (overloaded)"
        );
    }

    #[test]
    fn a_hit_limit_dialog_on_screen_is_a_refusal() {
        // The screen of the live poll on 2026-09-15 09:43, as `recent_text`
        // rendered it: the dialog, with the reset time in option 2.
        let screen = "────────────────────────────────\n\
                      What do you want to do?\n\
                      ❯1. Stop and wait for limit to reset\n\
                      2. Wait here, then continue automatically at Sep 17 at 10pm\n\
                      3. Switch to usage credits\n\
                      Entertoconfirm·Esctocancel\n";
        let refusal = refusal_on_screen(screen).expect("the dialog is a refusal");
        assert_eq!(refusal.kind, "rate_limit");
        assert_eq!(
            refusal.text,
            "Wait here, then continue automatically at Sep 17 at 10pm"
        );
        let message = explain_refusal(&account(), &refusal);
        assert!(message.starts_with("out of quota"), "{message}");
        assert!(message.contains("Sep 17 at 10pm"), "{message}");

        // The one-line notice, without a dialog.
        let notice = refusal_on_screen(
            "> reply with just: ok\n  ⎿ You've hit your weekly limit · resets Sep 17 at 10pm (Europe/Lisbon)\n",
        )
        .unwrap();
        assert_eq!(
            notice.text,
            "You've hit your weekly limit · resets Sep 17 at 10pm (Europe/Lisbon)"
        );

        // An ordinary answered turn, and a screen that merely mentions limits.
        assert_eq!(refusal_on_screen("> reply with just: ok\n⏺ ok\n"), None);
        assert_eq!(
            refusal_on_screen("Tip: /usage shows your limit windows\n"),
            None
        );
        assert_eq!(refusal_on_screen(""), None);
    }

    #[test]
    fn an_answered_turn_is_not_a_refusal() {
        let transcript = concat!(
            r#"{"type":"assistant","message":{"content":[{"type":"text","text":"ok"}]}}"#,
            "\n",
            "not json\n",
            r#"{"type":"assistant","isApiErrorMessage":false,"error":"none"}"#,
            "\n",
        );
        assert_eq!(refusal_from_transcript(transcript), None);
        assert_eq!(refusal_from_transcript(""), None);
        assert_eq!(latest_refusal(None), None);
        assert_eq!(
            latest_refusal(Some(Path::new("/nonexistent/transcript"))),
            None
        );
    }

    #[test]
    fn the_transcript_is_named_by_the_first_payload() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("cap.jsonl");
        std::fs::write(&capture, "{\"model\":{\"id\":\"a\"}}\n").unwrap();
        assert_eq!(transcript_path(&capture), None);
        std::fs::write(
            &capture,
            "{\"model\":{\"id\":\"a\"},\"transcript_path\":\"/h/projects/x/s.jsonl\"}\n",
        )
        .unwrap();
        assert_eq!(
            transcript_path(&capture),
            Some(PathBuf::from("/h/projects/x/s.jsonl"))
        );
    }

    #[test]
    fn an_unexplained_missing_payload_quotes_the_screen() {
        let message = explain_no_limits("  \n> reply with just: ok\n  ⏺ ok\n");
        assert!(
            message.starts_with("the session replied without rate limits"),
            "{message}"
        );
        assert!(
            message.contains("last on screen: > reply with just: ok ⏎ ⏺ ok"),
            "{message}"
        );
        assert!(!explain_no_limits("  \n").contains("last on screen"));
    }

    #[test]
    fn every_poll_launcher_forbids_an_inline_auto_update() {
        // A poll is on a clock and a release landing is a multi-minute
        // download, so `ovm <product>` must launch what is installed. Without
        // this, every Claude Code release blinded the poller until a human ran
        // the product by hand (2026-09-08, twice in one night).
        for product in ["claude", "codex"] {
            let launcher = crate::ovm_launcher(product);
            assert_eq!(launcher.get_program(), std::ffi::OsStr::new("ovm"));
            let sent: Vec<_> = launcher
                .get_envs()
                .filter(|(key, _)| *key == std::ffi::OsStr::new(crate::NO_AUTO_UPDATE_ENV))
                .collect();
            assert_eq!(
                sent.len(),
                1,
                "{product}: {} must be passed exactly once",
                crate::NO_AUTO_UPDATE_ENV
            );
            assert_eq!(
                sent[0].1,
                Some(std::ffi::OsStr::new("1")),
                "{product}: it must be set, not cleared"
            );
        }
    }

    #[test]
    fn statusline_windows_are_extracted_and_absent_ones_skipped() {
        let payload = json!({
            "rate_limits": {
                "five_hour": {"used_percentage": 19, "resets_at": 1788618000},
                "seven_day": {"used_percentage": 26.5, "resets_at": 1788771600}
            }
        });
        let windows = windows_from_statusline(&payload);
        assert_eq!(windows.len(), 2);
        assert_eq!(windows[0].id, "five_hour");
        assert_eq!(windows[0].label, "5h");
        assert_eq!(windows[0].used_percent, 19.0);
        assert_eq!(windows[0].resets_at, Some(1788618000));
        assert_eq!(windows[0].window_minutes, Some(300));
        assert_eq!(windows[1].used_percent, 26.5);
    }

    #[test]
    fn payload_without_rate_limits_yields_no_windows() {
        assert!(windows_from_statusline(&json!({"model": {"id": "x"}})).is_empty());
        assert!(windows_from_statusline(&json!({"rate_limits": {}})).is_empty());
    }

    #[test]
    fn spend_limit_may_exceed_one_hundred_percent() {
        let payload =
            json!({"rate_limits": {"spend_limit": {"used_percentage": 130, "resets_at": 1}}});
        let windows = windows_from_statusline(&payload);
        assert_eq!(windows[0].label, "spend");
        assert_eq!(windows[0].used_percent, 130.0);
        assert_eq!(windows[0].window_minutes, None);
    }

    #[test]
    fn claude_args_pin_the_model_the_capture_statusline_and_no_mcp() {
        let args = claude_args(&account(), Path::new("/tmp/it's here/cap.jsonl"));
        assert_eq!(&args[..2], ["--model", "haiku"]);
        assert_eq!(args[2], "--settings");
        let settings: Value = serde_json::from_str(&args[3]).unwrap();
        let command = settings["statusLine"]["command"].as_str().unwrap();
        assert!(
            command.starts_with("cat >> '/tmp/it'\\''s here/cap.jsonl'"),
            "{command}"
        );
        assert!(command.contains("printf '\\n'"));
        assert_eq!(args[4], "--strict-mcp-config");
    }

    #[test]
    fn a_configured_model_wins() {
        let mut account = account();
        account.model = Some("sonnet".into());
        assert_eq!(claude_args(&account, Path::new("/c"))[1], "sonnet");
    }

    #[test]
    fn an_isolated_home_keeps_its_own_claude_json_inside_it() {
        assert_eq!(
            claude_json_path(Path::new("/tmp/homes/claude-work")),
            PathBuf::from("/tmp/homes/claude-work/.claude.json")
        );
    }

    #[test]
    fn a_poll_refuses_the_home_the_person_works_in() {
        // The whole point of the isolated home: a poll refreshes the login it
        // runs as, so it must never run as the one the editor holds.
        let default = dirs::home_dir().expect("a home directory").join(".claude");
        let error = refuse_default_home(&default).expect_err("the default home must be refused");
        assert!(
            error
                .to_string()
                .contains("home your own Claude Code sessions use"),
            "{error}"
        );
        refuse_default_home(Path::new("/tmp/ovm-limits/homes/claude-default"))
            .expect("a home of our own is fine");
    }

    #[test]
    fn seeding_trust_preserves_everything_else_in_claude_json() {
        let temp = tempfile::tempdir().unwrap();
        let home = temp.path().join("home");
        std::fs::create_dir_all(&home).unwrap();
        let path = home.join(".claude.json");
        std::fs::write(
            &path,
            r#"{"hasCompletedOnboarding": true, "numStartups": 7, "projects": {"/other": {"allowedTools": ["Bash"]}}}"#,
        )
        .unwrap();
        let scratch = temp.path().join("scratch");
        seed_claude_json(&path, &scratch).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["numStartups"], 7);
        assert_eq!(root["projects"]["/other"]["allowedTools"][0], "Bash");
        assert_eq!(
            root["projects"][scratch.to_string_lossy().as_ref()]["hasTrustDialogAccepted"],
            true
        );
    }

    #[test]
    fn a_home_signed_in_by_login_still_gets_the_onboarding_flags() {
        // `claude auth login` leaves a file with the account and nothing else.
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("work").join(".claude.json");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, r#"{"oauthAccount":{"emailAddress":"x@y"}}"#).unwrap();
        seed_claude_json(&path, Path::new("/scratch")).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hasCompletedOnboarding"], true);
        assert!(root["lastOnboardingVersion"].is_string());
        assert_eq!(root["oauthAccount"]["emailAddress"], "x@y");
        assert_eq!(root["projects"]["/scratch"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn seeding_a_fresh_home_adds_onboarding_flags() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("fresh").join(".claude.json");
        seed_claude_json(&path, Path::new("/scratch")).unwrap();
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["hasCompletedOnboarding"], true);
        assert!(root["lastOnboardingVersion"].is_string());
        assert_eq!(root["projects"]["/scratch"]["hasTrustDialogAccepted"], true);
    }

    #[test]
    fn unseeding_removes_only_the_scratch_entry() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join(".claude.json");
        std::fs::write(
            &path,
            r#"{"numStartups": 3, "projects": {"/other": {"allowedTools": []}, "/scratch": {"hasTrustDialogAccepted": true}}}"#,
        )
        .unwrap();
        assert!(unseed_claude_json(&path, Path::new("/scratch")).unwrap());
        let root: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
        assert_eq!(root["numStartups"], 3);
        assert!(root["projects"].get("/other").is_some());
        assert!(root["projects"].get("/scratch").is_none());
        assert!(
            !unseed_claude_json(&path, Path::new("/scratch")).unwrap(),
            "second pass is a no-op"
        );
        assert!(
            !unseed_claude_json(Path::new("/nonexistent/.claude.json"), Path::new("/s")).unwrap()
        );
    }

    #[test]
    fn seeding_is_idempotent() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("home").join(".claude.json");
        seed_claude_json(&path, Path::new("/scratch")).unwrap();
        let first = std::fs::metadata(&path).unwrap().modified().unwrap();
        std::thread::sleep(Duration::from_millis(20));
        seed_claude_json(&path, Path::new("/scratch")).unwrap();
        let second = std::fs::metadata(&path).unwrap().modified().unwrap();
        assert_eq!(
            first, second,
            "an already-trusted scratch dir must not rewrite the file"
        );
    }

    #[test]
    fn the_latest_payload_with_windows_wins() {
        let temp = tempfile::tempdir().unwrap();
        let capture = temp.path().join("cap.jsonl");
        std::fs::write(
            &capture,
            concat!(
                r#"{"model": {"id": "a"}}"#,
                "\n",
                r#"{"rate_limits": {"five_hour": {"used_percentage": 10, "resets_at": 1}}}"#,
                "\n",
                r#"{"rate_limits": {"five_hour": {"used_percentage": 11, "resets_at": 1}}}"#,
                "\n",
                "\n",
                "not json\n",
            ),
        )
        .unwrap();
        assert_eq!(payload_count(&capture), 3);
        let latest = latest_payload_with_rate_limits(&capture).unwrap();
        assert_eq!(latest["rate_limits"]["five_hour"]["used_percentage"], 11);
    }

    #[test]
    fn login_details_tell_a_team_seat_from_an_own_plan() {
        let temp = tempfile::tempdir().unwrap();
        let write = |body: &str| std::fs::write(temp.path().join(".claude.json"), body).unwrap();
        write(
            r#"{"oauthAccount":{"accountUuid":"a","organizationType":"claude_max",
                "organizationRateLimitTier":"default_claude_max_20x"}}"#,
        );
        let own = login_details(temp.path()).unwrap();
        assert_eq!(own.kind, "personal");
        assert_eq!(
            own.rate_limit_tier.as_deref(),
            Some("default_claude_max_20x")
        );
        write(
            r#"{"oauthAccount":{"accountUuid":"a","organizationType":"claude_team",
                "organizationName":"Acme","organizationUuid":"org-1"}}"#,
        );
        let team = login_details(temp.path()).unwrap();
        assert_eq!(team.kind, "team");
        assert_eq!(team.org.as_deref(), Some("Acme"));
        assert_eq!(team.org_id.as_deref(), Some("org-1"));
        write(r#"{"oauthAccount":{"accountUuid":"a","seatTier":"premium"}}"#);
        assert_eq!(login_details(temp.path()).unwrap().kind, "team");
        write(r#"{"projects":{}}"#);
        assert!(login_details(temp.path()).is_none());
    }
}
