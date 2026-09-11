//! Polling a Codex account through `codex app-server`, the JSON-RPC surface
//! the Codex IDE extension uses. `account/rateLimits/read` returns every
//! window with its reset time; the credential stays inside the Codex binary
//! and the call costs no quota.

use crate::paths::LimitsDirs;
use crate::registry::{Account, Provider};
use crate::snapshot::{AccountSnapshot, Credits, ResetCredit, Window};
use crate::{LimitsError, Result};
use serde_json::{json, Value};
use std::io::{BufRead, BufReader, Write};
use std::os::unix::process::CommandExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

/// Override the launcher (tests point this at a fake). Invoked directly with
/// `app-server`; the default goes through `ovm codex`.
pub const LAUNCHER_ENV: &str = "OVM_LIMITS_CODEX_CMD";

pub const SOURCE: &str = "app-server";
const INIT_ID: u64 = 1;
const REQUEST_ID: u64 = 2;

pub fn poll(account: &Account, dirs: &LimitsDirs, timeout: Duration) -> Result<AccountSnapshot> {
    let home = account.poll_home(dirs);
    refuse_default_home(&home)?;
    std::fs::create_dir_all(&home)?;
    let mut command = launcher();
    command.arg("app-server");
    command.stdin(Stdio::piped());
    command.stdout(Stdio::piped());
    command.stderr(Stdio::piped());
    command.env(account.provider.home_env(), &home);
    // Own process group, so the cleanup below reaches the real codex when the
    // launcher (`ovm codex`) merely waits on it.
    command.process_group(0);
    let mut child = command.spawn().map_err(|error| {
        LimitsError::Message(format!("couldn't start codex app-server: {error}"))
    })?;

    let mut stdin = child.stdin.take().expect("piped stdin");
    let stdout = child.stdout.take().expect("piped stdout");
    let stderr = child.stderr.take().expect("piped stderr");

    let (sender, receiver) = mpsc::channel::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(|l| l.ok()) {
            if sender.send(line).is_err() {
                break;
            }
        }
    });
    // Whatever the launcher or codex complains about is the diagnosis when
    // the answer never comes; keep the tail, bounded.
    let stderr_tail = std::sync::Arc::new(std::sync::Mutex::new(std::collections::VecDeque::new()));
    let stderr_sink = std::sync::Arc::clone(&stderr_tail);
    std::thread::spawn(move || {
        for line in BufReader::new(stderr).lines().map_while(|l| l.ok()) {
            let mut tail = stderr_sink.lock().unwrap_or_else(|e| e.into_inner());
            if tail.len() >= 8 {
                tail.pop_front();
            }
            tail.push_back(line);
        }
    });
    let stderr_summary = {
        let tail = std::sync::Arc::clone(&stderr_tail);
        move || -> String {
            let tail = tail.lock().unwrap_or_else(|e| e.into_inner());
            let text = tail
                .iter()
                .map(|l| l.trim())
                .filter(|l| !l.is_empty())
                .collect::<Vec<_>>()
                .join(" | ");
            if text.is_empty() {
                String::new()
            } else {
                format!(" (stderr: {text})")
            }
        }
    };

    let outcome = (|| {
        for message in handshake_and_request() {
            writeln!(stdin, "{message}")?;
        }
        stdin.flush()?;
        let deadline = Instant::now() + timeout;
        loop {
            let remaining = deadline
                .checked_duration_since(Instant::now())
                .ok_or_else(|| {
                    LimitsError::Message(format!(
                        "codex app-server did not answer within {}s{}",
                        timeout.as_secs(),
                        stderr_summary()
                    ))
                })?;
            let line = receiver.recv_timeout(remaining).map_err(|error| {
                // Give a fast exit a moment to flush its stderr before we quote it.
                std::thread::sleep(Duration::from_millis(100));
                match error {
                    mpsc::RecvTimeoutError::Timeout => LimitsError::Message(format!(
                        "codex app-server did not answer within {}s{}",
                        timeout.as_secs(),
                        stderr_summary()
                    )),
                    mpsc::RecvTimeoutError::Disconnected => LimitsError::Message(format!(
                        "codex app-server exited before answering{}",
                        stderr_summary()
                    )),
                }
            })?;
            let Ok(message) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let id = message.get("id").and_then(Value::as_u64);
            if id == Some(INIT_ID) {
                if let Some(error) = message.get("error") {
                    let text = error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("unknown error");
                    return Err(LimitsError::Message(format!(
                        "codex app-server refused to initialize: {text}"
                    )));
                }
                continue;
            }
            if id != Some(REQUEST_ID) {
                continue;
            }
            if let Some(error) = message.get("error") {
                let text = error
                    .get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("unknown error");
                return Err(LimitsError::Message(format!(
                    "codex refused account/rateLimits/read: {text} (signed out?)"
                )));
            }
            let result = message
                .get("result")
                .cloned()
                .ok_or_else(|| LimitsError::Message("codex answered without a result".into()))?;
            return Ok(snapshot_from_result(account, &result));
        }
    })();

    drop(stdin);
    // SAFETY: signalling the process group this function created.
    unsafe {
        libc::killpg(child.id() as libc::pid_t, libc::SIGKILL);
    }
    let _ = child.kill();
    let _ = child.wait();
    outcome
}

/// The command that reaches this account's Codex, for `login`.
pub fn product_command(account: &Account, home: &Path) -> Command {
    let mut command = launcher();
    command.env(account.provider.home_env(), home);
    command
}

/// Codex refreshes `auth.json` as it answers, so the same rule as Claude:
/// a poll never runs in the home your own `codex` command uses.
pub fn refuse_default_home(home: &Path) -> Result<()> {
    if Provider::Codex.is_default_home(home) {
        return Err(LimitsError::Message(format!(
            "refusing to poll {}: that is the home your own Codex sessions use. Give the account a \
             home of its own and sign in there once.",
            crate::paths::display(home)
        )));
    }
    Ok(())
}

fn launcher() -> Command {
    match std::env::var_os(LAUNCHER_ENV).filter(|value| !value.is_empty()) {
        Some(path) => Command::new(path),
        // The app-server has 60s to answer; an inline auto-update download
        // would spend all of it and more. See the Claude launcher.
        None => crate::ovm_launcher("codex"),
    }
}

/// The three lines written to the app-server, in order.
pub fn handshake_and_request() -> [String; 3] {
    [
        json!({
            "jsonrpc": "2.0", "id": INIT_ID, "method": "initialize",
            "params": {"clientInfo": {"name": "ovm-limits", "title": "ovm limits", "version": env!("CARGO_PKG_VERSION")}}
        })
        .to_string(),
        json!({"jsonrpc": "2.0", "method": "initialized"}).to_string(),
        json!({"jsonrpc": "2.0", "id": REQUEST_ID, "method": "account/rateLimits/read", "params": {}})
            .to_string(),
    ]
}

/// Pure: `account/rateLimits/read` result → windows. Prefers the per-limit
/// map (one entry per model family) and falls back to the single top-level
/// `rateLimits` older servers return.
pub fn windows_from_result(result: &Value) -> Vec<Window> {
    let mut windows = Vec::new();
    if let Some(by_id) = result.get("rateLimitsByLimitId").and_then(Value::as_object) {
        for (limit_id, limit) in by_id {
            push_limit(&mut windows, limit_id, limit);
        }
    } else if let Some(limit) = result.get("rateLimits") {
        let limit_id = limit
            .get("limitId")
            .and_then(Value::as_str)
            .unwrap_or("codex");
        push_limit(&mut windows, limit_id, limit);
    }
    windows
}

fn push_limit(windows: &mut Vec<Window>, limit_id: &str, limit: &Value) {
    let name = limit
        .get("limitName")
        .and_then(Value::as_str)
        .filter(|n| !n.is_empty())
        .unwrap_or(limit_id);
    for (slot, key) in [("primary", "primary"), ("secondary", "secondary")] {
        let Some(window) = limit.get(key).filter(|w| !w.is_null()) else {
            continue;
        };
        let Some(used) = window.get("usedPercent").and_then(Value::as_f64) else {
            continue;
        };
        let minutes = window.get("windowDurationMins").and_then(Value::as_u64);
        windows.push(Window {
            id: format!("{limit_id}.{slot}"),
            label: format!("{name} {}", duration_label(minutes)),
            used_percent: used,
            resets_at: window.get("resetsAt").and_then(Value::as_u64),
            window_minutes: minutes,
            last_reset_at: None,
        });
    }
}

pub fn duration_label(minutes: Option<u64>) -> String {
    match minutes {
        Some(m) if m % (7 * 24 * 60) == 0 => format!("{}w", m / (7 * 24 * 60)),
        Some(m) if m % (24 * 60) == 0 => format!("{}d", m / (24 * 60)),
        Some(m) if m % 60 == 0 => format!("{}h", m / 60),
        Some(m) => format!("{m}m"),
        None => "window".into(),
    }
}

/// Everything `account/rateLimits/read` says, typed where this tool
/// understands it and verbatim under `raw` where it does not.
pub fn snapshot_from_result(account: &Account, result: &Value) -> AccountSnapshot {
    let limits = result.get("rateLimits").cloned().unwrap_or(Value::Null);
    let text = |value: &Value, key: &str| value.get(key).and_then(Value::as_str).map(str::to_owned);
    let credits = limits
        .get("credits")
        .filter(|c| c.is_object())
        .map(|c| Credits {
            has_credits: c
                .get("hasCredits")
                .and_then(Value::as_bool)
                .unwrap_or(false),
            unlimited: c.get("unlimited").and_then(Value::as_bool).unwrap_or(false),
            balance: text(c, "balance"),
        });
    let reset_credits = result
        .pointer("/rateLimitResetCredits/credits")
        .and_then(Value::as_array)
        .map(|list| {
            list.iter()
                .filter_map(|c| {
                    Some(ResetCredit {
                        id: text(c, "id")?,
                        reset_type: text(c, "resetType"),
                        status: text(c, "status"),
                        granted_at: c.get("grantedAt").and_then(Value::as_u64),
                        expires_at: c.get("expiresAt").and_then(Value::as_u64),
                        title: text(c, "title"),
                        description: text(c, "description"),
                    })
                })
                .collect()
        })
        .unwrap_or_default();
    AccountSnapshot {
        plan: text(&limits, "planType"),
        windows: windows_from_result(result),
        reset_credits_available: result
            .pointer("/rateLimitResetCredits/availableCount")
            .and_then(Value::as_u64),
        account_id: text(result, "accountId"),
        credits,
        spend_control_reached: limits.get("spendControlReached").and_then(Value::as_bool),
        rate_limit_reached_type: text(&limits, "rateLimitReachedType"),
        reset_credits,
        raw: Some(result.clone()),
        ..AccountSnapshot::for_account(account, SOURCE)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::registry::Provider;

    fn account() -> Account {
        Account::new(Provider::Codex, "codex-1", None)
    }

    /// Shape captured from codex-cli 0.153.4 on 2026-09-05.
    fn sample() -> Value {
        json!({
            "rateLimits": {
                "limitId": "codex", "limitName": null,
                "primary": {"usedPercent": 19, "windowDurationMins": 10080, "resetsAt": 1789192025},
                "secondary": null,
                "planType": "pro"
            },
            "rateLimitsByLimitId": {
                "codex": {
                    "limitId": "codex", "limitName": null,
                    "primary": {"usedPercent": 19, "windowDurationMins": 10080, "resetsAt": 1789192025},
                    "secondary": null, "planType": "pro"
                },
                "codex_bengalfox": {
                    "limitId": "codex_bengalfox", "limitName": "GPT-5.3-Codex-Spark",
                    "primary": {"usedPercent": 0, "windowDurationMins": 300, "resetsAt": 1788626583},
                    "secondary": {"usedPercent": 0, "windowDurationMins": 10080, "resetsAt": 1789213383},
                    "planType": "pro"
                }
            },
            "rateLimitResetCredits": {"availableCount": 1, "credits": [{
                "id": "RateLimitResetCredit_1", "resetType": "codexRateLimits", "status": "available",
                "grantedAt": 1788581906, "expiresAt": 1791173906,
                "title": "Full reset", "description": "one free reset"
            }]},
            "accountId": "acct-1"
        })
    }

    #[test]
    fn every_limit_and_slot_becomes_a_window() {
        let windows = windows_from_result(&sample());
        let ids: Vec<&str> = windows.iter().map(|w| w.id.as_str()).collect();
        assert_eq!(
            ids,
            [
                "codex.primary",
                "codex_bengalfox.primary",
                "codex_bengalfox.secondary"
            ]
        );
        assert_eq!(windows[0].label, "codex 1w");
        assert_eq!(windows[0].used_percent, 19.0);
        assert_eq!(windows[0].resets_at, Some(1789192025));
        assert_eq!(windows[1].label, "GPT-5.3-Codex-Spark 5h");
        assert_eq!(windows[2].window_minutes, Some(10080));
    }

    #[test]
    fn older_servers_without_the_map_still_yield_the_top_level_limit() {
        let mut result = sample();
        result
            .as_object_mut()
            .unwrap()
            .remove("rateLimitsByLimitId");
        let windows = windows_from_result(&result);
        assert_eq!(windows.len(), 1);
        assert_eq!(windows[0].id, "codex.primary");
    }

    #[test]
    fn snapshot_carries_everything_the_server_said() {
        let mut sample = sample();
        sample["rateLimits"]["credits"] =
            json!({"hasCredits": false, "unlimited": false, "balance": "0"});
        sample["rateLimits"]["spendControlReached"] = json!(false);
        let snapshot = snapshot_from_result(&account(), &sample);
        assert_eq!(snapshot.plan.as_deref(), Some("pro"));
        assert_eq!(snapshot.reset_credits_available, Some(1));
        assert_eq!(snapshot.source, SOURCE);
        assert!(snapshot.error.is_none());
        assert_eq!(snapshot.account_id.as_deref(), Some("acct-1"));
        assert_eq!(
            snapshot.credits,
            Some(Credits {
                has_credits: false,
                unlimited: false,
                balance: Some("0".into())
            })
        );
        assert_eq!(snapshot.spend_control_reached, Some(false));
        assert_eq!(snapshot.rate_limit_reached_type, None);
        assert_eq!(snapshot.reset_credits.len(), 1);
        let credit = &snapshot.reset_credits[0];
        assert_eq!(credit.title.as_deref(), Some("Full reset"));
        assert_eq!(credit.status.as_deref(), Some("available"));
        assert_eq!(credit.expires_at, Some(1791173906));
        // And the whole result rides along untouched for whatever this tool
        // does not model yet.
        assert_eq!(snapshot.raw.as_ref(), Some(&sample));
    }

    #[test]
    fn duration_labels_read_like_a_person_wrote_them() {
        assert_eq!(duration_label(Some(300)), "5h");
        assert_eq!(duration_label(Some(10080)), "1w");
        assert_eq!(duration_label(Some(1440)), "1d");
        assert_eq!(duration_label(Some(90)), "90m");
        assert_eq!(duration_label(None), "window");
    }

    #[test]
    fn the_request_is_three_well_formed_jsonrpc_lines() {
        let lines = handshake_and_request();
        let init: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(init["method"], "initialize");
        assert_eq!(init["id"], 1);
        let read: Value = serde_json::from_str(&lines[2]).unwrap();
        assert_eq!(read["method"], "account/rateLimits/read");
        assert_eq!(read["id"], REQUEST_ID);
    }
}
